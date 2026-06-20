//! 自建 agent loop (非 Agent SDK, 架构 §1)。
//!
//! 数据流 (一个 turn): full 原文 log (canonical) ──投影+normalize──► wire messages
//! ──client.chat (含 ② 重试)──► assistant 全文回填 canonical ──若 tool_calls 则分发并回填结果──►
//! 直到 `finish_reason != ToolCalls`。裁切只发生在「发送投影」一侧, canonical 永远存全文 (D1)。
//!
//! 可选挂 [`SessionStore`] 做持久化 (D2/D4): 每条消息 append 落库, turn 起点打 checkpoint;
//! `resume_session` 从库重建内存 Session。

use crate::background::BackgroundRegistry;
use crate::context::ContextManager;
use crate::file_state::FileStateCache;
use crate::fs_scope::SharedFsScope;
use crate::permission::{AllowAll, Approver, Decision};
use crate::registry::ToolRegistry;
use crate::session::Session;
use crate::state::SessionStore;
use crate::tool::ToolCtx;
use std::path::PathBuf;
use std::sync::Arc;
use syncode_lsp::LspManager;
use syncode_llm::client::{DeepSeekClient, MODEL};
use syncode_llm::context::normalize_for_api;
use syncode_llm::wire::{
    ChatRequest, FinishReason, FunctionCall, Message, ReasoningEffort, Thinking, ThinkingType,
    ToolCall,
};

/// 一个 turn 的自建循环: 投影裁切 → 请求 → 若 `tool_calls` 则分发执行 → 回填 → 直到 `stop`。
pub struct AgentLoop {
    client: Arc<DeepSeekClient>,
    tools: ToolRegistry,
    context: ContextManager,
    approver: Arc<dyn Approver>,
    /// 跨工具共享的文件读状态缓存 (Read 写 / Edit/Write 读, §10)。
    files: Arc<FileStateCache>,
    /// 跨工具共享的 LSP 客户端管理器 (常驻语言服务器复用, §4/§6.2)。
    lsp: Arc<LspManager>,
    /// 文件写收容守卫 (P1c)。无则不收容 (写类工具裸 std::fs)。
    fs: SharedFsScope,
    /// 共享后台任务注册表 (§5.5): 全 turn 复用, 后台任务跨 dispatch 可查/可杀。
    background: Arc<BackgroundRegistry>,
    /// 授权项目根 = 工具 cwd 的**单一真相** (review fix #14): 与 approver / fs-scope 同一值, 别每次
    /// dispatch 重读 `std::env::current_dir()` (那会与两闸钉死的根漂移)。
    cwd: PathBuf,
    /// 可选持久化 (D2/D4)。无则纯内存。
    store: Option<SessionStore>,
    session_id: String,
}

impl AgentLoop {
    pub fn new(client: Arc<DeepSeekClient>, tools: ToolRegistry) -> Self {
        Self {
            client,
            tools,
            context: ContextManager::default(),
            approver: Arc::new(AllowAll),
            files: Arc::new(FileStateCache::new()),
            lsp: Arc::new(LspManager::new()),
            fs: None,
            background: Arc::new(BackgroundRegistry::new()),
            cwd: std::env::current_dir().unwrap_or_default(),
            store: None,
            session_id: "default".to_string(),
        }
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// 挂上文件写收容守卫 (P1c): 写类工具落盘前校验路径在授权根内。
    pub fn with_fs_scope(mut self, fs: SharedFsScope) -> Self {
        self.fs = fs;
        self
    }

    /// 钉死工具 cwd = 授权项目根 (应传与 approver / fs-scope 相同的 root, 单一真相, review fix #14)。
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    pub fn with_context(mut self, context: ContextManager) -> Self {
        self.context = context;
        self
    }

    /// 挂上持久化: 每条消息落库 + turn 起点打 checkpoint (D2/D4)。
    pub fn with_store(mut self, store: SessionStore, session_id: impl Into<String>) -> Self {
        self.store = Some(store);
        self.session_id = session_id.into();
        self
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// 从持久化 store 重建内存 Session (resume)。无 store 时返回空 Session。
    pub fn resume_session(&self) -> Session {
        match &self.store {
            Some(store) => {
                Session::from_messages(store.load(&self.session_id).unwrap_or_default())
            }
            None => Session::new(),
        }
    }

    /// 组装一次请求: 从 canonical 投影出裁切后的 wire messages + 结构 normalize (③),
    /// 思考模式 + max 强度 (复杂 agent 场景, §7.1), 带全部工具定义。
    fn build_request(&self, session: &Session) -> ChatRequest {
        let wire = normalize_for_api(&self.context.project(session.messages()));
        ChatRequest {
            model: MODEL.to_string(),
            messages: wire,
            thinking: Some(Thinking { kind: ThinkingType::Enabled }),
            reasoning_effort: Some(ReasoningEffort::Max),
            // 无工具时不发 tools 字段; 思考模式下 tool_choice 只能 None/auto/none (传 required 会 400)。
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(self.tools.definitions())
            },
            tool_choice: None,
            max_tokens: Some(8192),
            temperature: None,
            stop: None,
            stream: false,
            response_format: None,
            stream_options: None,
        }
    }

    /// 把消息推进 canonical 内存 Session, 并 (若挂了 store) 落库。持久化失败不阻断 loop。
    fn commit(&self, session: &mut Session, message: Message) {
        if let Some(store) = &self.store {
            if let Err(e) = store.append(&self.session_id, &message) {
                eprintln!("[syncode] persist failed (continuing in-memory): {e}");
            }
        }
        session.push(message);
    }

    /// turn 起点: 把 run_turn 之前加入 session 但尚未落库的尾部消息补齐 (如 push_user 的 user 轮),
    /// 然后打一个 checkpoint (= 一个可回滚点)。
    fn sync_and_checkpoint(&self, session: &Session) {
        let Some(store) = &self.store else { return };
        if let Ok(persisted) = store.len(&self.session_id) {
            for m in session.messages().iter().skip(persisted) {
                let _ = store.append(&self.session_id, m);
            }
        }
        let _ = store.checkpoint(&self.session_id, "turn");
    }

    /// 跑一个完整 turn 直到模型给出非 `ToolCalls`(且非 leaked tool-call)的 `finish_reason`。
    pub async fn run_turn(&mut self, session: &mut Session) -> syncode_llm::Result<()> {
        self.sync_and_checkpoint(session);
        loop {
            let request = self.build_request(session);
            let response = self.client.chat(&request).await?;
            let choice = response
                .choices
                .into_iter()
                .next()
                .ok_or(syncode_llm::Error::EmptyResponse)?;
            let finish = choice.finish_reason;
            let message = choice.message;

            // 后端算力不足被中断 (§16): 不采纳本次输出, 重发。(TODO: 加重试上限。)
            if finish == Some(FinishReason::InsufficientSystemResource) {
                continue;
            }

            // assistant 全文 (含完整 reasoning_content) 回填 canonical。裁切只在发送投影侧做 (D1)。
            let tool_calls = message.tool_calls.clone().unwrap_or_default();
            let content = message.content.clone().unwrap_or_default();
            self.commit(session, message);

            if finish == Some(FinishReason::ToolCalls) && !tool_calls.is_empty() {
                // 并行工具调用: 一轮可有多个 tool_calls, 逐个执行并回填结果 (§11)。
                for tc in &tool_calls {
                    let result = self.dispatch_tool(tc).await;
                    self.commit(session, Message::tool_result(tc.id.as_str(), result));
                }
                continue; // 带着工具结果再进下一轮
            }

            // §8 坑#2: DeepSeek 偶发把工具调用当文本吐在 content 且 finish_reason=stop。
            // 保守恢复守卫: 若 content 整体是一个「已注册工具」的 JSON 调用, 当作 tool_calls 处理。
            if finish == Some(FinishReason::Stop) {
                if let Some(tc) = detect_leaked_tool_call(&content, &self.tools) {
                    let result = self.dispatch_tool(&tc).await;
                    self.commit(session, Message::tool_result(tc.id.as_str(), result));
                    continue;
                }
            }

            return Ok(()); // Stop / Length / ContentFilter → turn 结束
        }
    }

    /// 分发一次工具调用, 返回回给模型的结果字符串。
    /// error message 写给模型读 (借鉴 CC `<tool_use_error>` 包裹, 利于自纠偏 §10)。
    async fn dispatch_tool(&self, tc: &ToolCall) -> String {
        let name = tc.function.name.as_str();
        let Some(tool) = self.tools.get(name) else {
            return format!("<tool_use_error>No such tool available: {name}</tool_use_error>");
        };
        let args = match serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
            Ok(v) => v,
            Err(e) => {
                return format!("<tool_use_error>invalid tool arguments JSON: {e}</tool_use_error>");
            }
        };
        // 危险动作走审批 (§7.5): 工具据 args **自分类** (Bash 按命令、Write/Edit 按目标路径),
        // 审批器按可逆性 / 影响面判。`classify` 返回 `None` = 安全, 不过闸。
        // **穷举 match (无通配)**: Ask 在没有人类审批通道前 fail-closed —— 绝不静默放行
        // (否则接入真审批器时 Ask 会塌成 Allow, 任意命令无提示执行, 破坏放权底座)。
        if let Some(req) = tool.classify(&args) {
            match self.approver.decide(&req) {
                Decision::Allow => {}
                Decision::Deny => {
                    return format!(
                        "<tool_use_error>permission denied: {name} ({:?}) is blocked by the approver policy</tool_use_error>",
                        req.class
                    );
                }
                Decision::Ask => {
                    let what = req.target.as_deref().unwrap_or(name);
                    return format!(
                        "<tool_use_error>{name} needs human approval for a {:?} action ({what}), but \
                         interactive approval is not wired up yet, so it was refused. If this is safe and \
                         intended, pre-authorize it in the approver policy (e.g. an allowed write root).</tool_use_error>",
                        req.class
                    );
                }
            }
        }
        let mut ctx = ToolCtx::with_lsp(self.files.clone(), self.cwd.clone(), self.lsp.clone());
        ctx.fs = self.fs.clone();
        ctx.background = self.background.clone();
        match tool.call(args, &ctx).await {
            Ok(out) if out.is_error => format!("<tool_use_error>{}</tool_use_error>", out.content),
            Ok(out) => out.content,
            Err(e) => format!("<tool_use_error>{e}</tool_use_error>"),
        }
    }
}

/// §8 坑#2 恢复守卫 (纯函数): 若 `content` 整段就是一个「已注册工具」的 JSON 调用
/// (`{"name": <已注册工具>, "arguments": {...}|"..."}`), 还原成 [`ToolCall`]。**保守**:
/// 要求整段是合法 JSON 对象、`name` 命中已注册工具 —— 避免把正常回答里的 JSON 误判成调用。
fn detect_leaked_tool_call(content: &str, tools: &ToolRegistry) -> Option<ToolCall> {
    let v: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    let obj = v.as_object()?;
    let name = obj.get("name")?.as_str()?;
    tools.get(name)?; // 必须是已注册工具
    let arguments = match obj.get("arguments") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).ok()?,
        None => "{}".to_string(),
    };
    Some(ToolCall {
        id: "recovered_leaked_call".to_string(),
        kind: "function".to_string(),
        function: FunctionCall { name: name.to_string(), arguments },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolError, ToolOutput};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use syncode_llm::{DeepSeekClient, DeepSeekConfig};

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes its `text` argument"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false})
        }
        async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            Ok(ToolOutput::ok(format!("echo: {text}")))
        }
    }

    fn dummy_client() -> Arc<DeepSeekClient> {
        Arc::new(DeepSeekClient::new(DeepSeekConfig::new("dummy-key")).unwrap())
    }
    fn registry_with(tool: Arc<dyn Tool>) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(tool);
        reg
    }
    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall { name: name.to_string(), arguments: args.to_string() },
        }
    }

    #[tokio::test]
    async fn dispatch_known_tool_runs() {
        let agent = AgentLoop::new(dummy_client(), registry_with(Arc::new(EchoTool)));
        let out = agent.dispatch_tool(&call("echo", r#"{"text":"hi"}"#)).await;
        assert_eq!(out, "echo: hi");
    }

    /// 测试工具: 把自己分类成指定 [`ActionClass`], call 时翻 ran 标记 —— 验证审批闸是否真挡住执行。
    struct ClassTool {
        class: crate::permission::ActionClass,
        target: Option<String>,
        ran: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl Tool for ClassTool {
        fn name(&self) -> &str {
            "danger"
        }
        fn description(&self) -> &str {
            "a tool that classifies itself for the approver"
        }
        fn classify(&self, _args: &Value) -> Option<crate::permission::ActionRequest> {
            let mut req = crate::permission::ActionRequest::new(self.class.clone(), "danger");
            req.target = self.target.clone();
            Some(req)
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{},"additionalProperties":false})
        }
        async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::ok("ran"))
        }
    }

    fn class_tool(
        class: crate::permission::ActionClass,
        target: Option<&str>,
    ) -> (Arc<dyn Tool>, Arc<std::sync::atomic::AtomicBool>) {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tool = Arc::new(ClassTool { class, target: target.map(str::to_string), ran: ran.clone() });
        (tool, ran)
    }

    struct AskAll;
    impl crate::permission::Approver for AskAll {
        fn decide(&self, _req: &crate::permission::ActionRequest) -> crate::permission::Decision {
            crate::permission::Decision::Ask
        }
    }

    #[tokio::test]
    async fn ask_decision_fails_closed_and_does_not_execute() {
        let (tool, ran) = class_tool(crate::permission::ActionClass::ArbitraryExec, None);
        let agent =
            AgentLoop::new(dummy_client(), registry_with(tool)).with_approver(Arc::new(AskAll));
        let out = agent.dispatch_tool(&call("danger", "{}")).await;
        assert!(out.contains("needs human approval"), "got: {out}");
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "dangerous tool must NOT execute on Ask"
        );
    }

    #[tokio::test]
    async fn policy_allows_safe_class_and_runs() {
        // Build (可逆/项目内) → PolicyApprover 放行 → 工具真执行。
        let (tool, ran) = class_tool(crate::permission::ActionClass::Build, None);
        let agent = AgentLoop::new(dummy_client(), registry_with(tool))
            .with_approver(Arc::new(crate::permission::PolicyApprover::new("/proj")));
        let out = agent.dispatch_tool(&call("danger", "{}")).await;
        assert_eq!(out, "ran");
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst), "safe class must execute");
    }

    #[tokio::test]
    async fn policy_refuses_outward_class() {
        // Network (外发) → PolicyApprover Ask → fail-closed 拒, 工具不执行。
        let (tool, ran) = class_tool(crate::permission::ActionClass::Network, None);
        let agent = AgentLoop::new(dummy_client(), registry_with(tool))
            .with_approver(Arc::new(crate::permission::PolicyApprover::new("/proj")));
        let out = agent.dispatch_tool(&call("danger", "{}")).await;
        assert!(out.contains("needs human approval"), "got: {out}");
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "outward class must NOT execute");
    }

    #[tokio::test]
    async fn policy_refuses_write_outside_root() {
        // WriteFs 根外 → Ask → 拒。
        let (tool, ran) =
            class_tool(crate::permission::ActionClass::WriteFs, Some("/etc/cron.d/evil"));
        let agent = AgentLoop::new(dummy_client(), registry_with(tool))
            .with_approver(Arc::new(crate::permission::PolicyApprover::new("/proj")));
        let out = agent.dispatch_tool(&call("danger", "{}")).await;
        assert!(out.contains("needs human approval"), "got: {out}");
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_model_readable_error() {
        let agent = AgentLoop::new(dummy_client(), registry_with(Arc::new(EchoTool)));
        let out = agent.dispatch_tool(&call("nope", "{}")).await;
        assert!(out.contains("No such tool available: nope"), "got: {out}");
    }

    #[tokio::test]
    async fn dispatch_bad_json_args_returns_error() {
        let agent = AgentLoop::new(dummy_client(), registry_with(Arc::new(EchoTool)));
        let out = agent.dispatch_tool(&call("echo", "{not json")).await;
        assert!(out.contains("invalid tool arguments JSON"), "got: {out}");
    }

    #[test]
    fn persistence_and_resume_roundtrip() {
        let store = SessionStore::in_memory().unwrap();
        let agent = AgentLoop::new(dummy_client(), ToolRegistry::new()).with_store(store, "s1");
        let mut session = Session::with_system("sys");
        session.push_user("hi"); // run_turn 之前加入
        agent.sync_and_checkpoint(&session); // 补齐 system+user 落库
        agent.commit(&mut session, Message::user("more"));
        // resume: 从库重建, 与内存一致
        let resumed = agent.resume_session();
        assert_eq!(resumed.messages().len(), 3);
        assert_eq!(resumed.messages()[0].content.as_deref(), Some("sys"));
        assert_eq!(resumed.messages()[2].content.as_deref(), Some("more"));
    }

    #[test]
    fn detect_leaked_tool_call_recognizes_registered_tool() {
        let reg = registry_with(Arc::new(EchoTool));
        let leaked = r#"{"name":"echo","arguments":{"text":"hi"}}"#;
        let tc = detect_leaked_tool_call(leaked, &reg).expect("should detect");
        assert_eq!(tc.function.name, "echo");
        assert!(tc.function.arguments.contains("text"));
    }

    #[test]
    fn detect_leaked_ignores_normal_text() {
        let reg = registry_with(Arc::new(EchoTool));
        assert!(detect_leaked_tool_call("Hello! How can I help you today?", &reg).is_none());
    }

    #[test]
    fn detect_leaked_ignores_unregistered_tool_name() {
        let reg = ToolRegistry::new(); // 空: echo 未注册
        let leaked = r#"{"name":"echo","arguments":{}}"#;
        assert!(detect_leaked_tool_call(leaked, &reg).is_none());
    }
}
