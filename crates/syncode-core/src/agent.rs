//! 自建 agent loop (非 Agent SDK, 架构 §1)。
//!
//! 数据流 (一个 turn): full 原文 log (canonical) ──投影+normalize──► wire messages
//! ──client.chat (含 ② 重试)──► assistant 全文回填 canonical ──若 tool_calls 则分发并回填结果──►
//! 直到 `finish_reason != ToolCalls`。裁切只发生在「发送投影」一侧, canonical 永远存全文 (D1)。
//!
//! 可选挂 [`SessionStore`] 做持久化 (D2/D4): 每条消息 append 落库, turn 起点打 checkpoint;
//! `resume_session` 从库重建内存 Session。

use crate::context::ContextManager;
use crate::file_state::FileStateCache;
use crate::permission::{ActionClass, AllowAll, Approver, Decision};
use crate::registry::ToolRegistry;
use crate::session::Session;
use crate::state::SessionStore;
use crate::tool::ToolCtx;
use std::sync::Arc;
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
            store: None,
            session_id: "default".to_string(),
        }
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
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
        // 危险工具走审批 (§7)。当前粗粒度按 ArbitraryExec; 后续按工具语义细分 ActionClass。
        if tool.is_dangerous() && self.approver.decide(&ActionClass::ArbitraryExec) == Decision::Deny
        {
            return format!("<tool_use_error>permission denied for {name}</tool_use_error>");
        }
        let ctx = ToolCtx::new(self.files.clone(), std::env::current_dir().unwrap_or_default());
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
