//! 自建 agent loop (非 Agent SDK, 架构 §1)。
//!
//! 数据流 (一个 turn): full 原文 log (canonical) ──投影+normalize──► wire messages
//! ──client.chat (含 ② 重试)──► assistant 全文回填 canonical ──若 tool_calls 则分发并回填结果──►
//! 直到 `finish_reason != ToolCalls`。裁切只发生在「发送投影」一侧, canonical 永远存全文 (D1)。
//!
//! 可选挂 [`SessionStore`] 做持久化 (D2/D4): 每条消息 append 落库, turn 起点打 checkpoint;
//! `resume_session` 从库重建内存 Session。

use crate::background::BackgroundRegistry;
use crate::compaction::{
    assemble_with_summary, first_non_system_index, flatten_for_summary, project_window,
    protected_tail_start, Budget, KEEP_RECENT_TURNS,
};
use crate::context::ContextManager;
use crate::file_state::FileStateCache;
use crate::fs_scope::SharedFsScope;
use crate::permission::{AllowAll, Approver, Decision};
use crate::registry::ToolRegistry;
use crate::session::Session;
use crate::state::SessionStore;
use crate::tool::{SubAgentRequest, SubAgentSpawner, TodoItem, ToolCtx};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use syncode_lsp::LspManager;
use syncode_llm::client::{DeepSeekClient, MODEL};
use syncode_llm::stream::ChatStreamChunk;
use syncode_llm::wire::{
    ChatRequest, FinishReason, FunctionCall, Message, ReasoningEffort, Thinking, ThinkingType,
    ToolCall,
};

/// agent loop 跑动时发出的进度事件 (供 UI 流式渲染)。回调式投递 (core 不绑定具体 channel):
/// UI 用 [`with_event_sink`](AgentLoop::with_event_sink) 注入一个闭包, 把事件推进自己的通道。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 模型本轮可见文本 (assistant content) —— **非流式整段** (worker 用它发错误/提示)。
    AssistantText(String),
    /// 流式: assistant 文本的一个增量片段 (逐字追加到当前行)。
    AssistantDelta(String),
    /// 流式: reasoning (CoT) 的一个增量片段。
    ReasoningDelta(String),
    /// 模型本轮的推理 (CoT) 全文; UI 自行折叠/截断展示。
    Reasoning { text: String },
    /// 一个工具即将执行。
    ToolStarted { name: String, args: String },
    /// 工具返回 (完整结果文本; UI 自行决定折叠/截断展示)。
    ToolFinished { name: String, result: String, is_error: bool },
    /// 编辑类工具改了文件: 携带 unified diff (供 UI 渲染 diff 视图)。
    FileChanged { path: String, diff: String },
    /// 一次 API 响应的 token 用量 (DeepSeek usage)。一个 turn 可能多次 (每轮工具往返各一次);
    /// `cache_hit_tokens` = prompt 命中前缀缓存的部分, `reasoning_tokens` = CoT 消耗 (含在 completion 内)。
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cache_hit_tokens: u64,
        reasoning_tokens: u64,
    },
    /// 触发了一次自动 context 压缩 (爬上 Baseline 以上的档): `rung` = 档位名,
    /// `before`/`after` = 压缩前后的 token 估算 (供 UI / eval 观测压缩决策)。
    Compacted { rung: String, before: u64, after: u64 },
    /// 任务清单更新 (TodoWrite): 携带更新后的完整清单, 供 UI 渲染清单面板。
    Todos(Vec<TodoItem>),
    /// turn 正常结束。
    TurnDone,
    /// turn 被用户中止 (Stop)。会话已被 `repair_after_interrupt` 修复成可继续状态。
    Interrupted,
}

/// 后端 `InsufficientSystemResource` 的重试上限 (防永久空转, §16; 原 TODO)。
const MAX_RESOURCE_RETRIES: u32 = 5;

/// LLM 摘要的连续失败熔断阈值 (借鉴 CC): 连失这么多次就不再尝试摘要 (退回纯窗口投影)。
const MAX_SUMMARIZE_FAILS: u32 = 3;
/// 摘要请求的 max_tokens 上限。
const SUMMARY_MAX_TOKENS: u32 = 32_000;
/// context 过长的 400 反应式重试上限 (每次先强制摘要再重发, 防永久空转)。
const MAX_SUMMARY_RETRIES: u32 = 2;

/// 事件回调汇 (在 agent 的 async 上下文里**同步**调用; 实现应是非阻塞的, 如往 channel 投递)。
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Ask 升级钩子 (交互档审批, §5.1): 当**同步**策略审批器判 `Ask` 且装了此钩子时, agent `await`
/// 它拿人的裁决 (只会是 Allow/Deny —— Allow once)。**无钩子 = `Ask` fail-closed 拒** (headless /
/// CLI 行为零变化)。策略审批器仍是权威, 此钩子只处理 `Ask` 档的「叫人」升级。
///
/// 用 `Pin<Box<dyn Future>>` 而非 async-trait, 避免引第三方 future 依赖; 返回的 future 必须 `Send`
/// (agent 跑在多线程 tokio runtime)。UI 实现里它通常: 发审批请求给 UI + await 一个回信通道。
pub type AskGate = Arc<
    dyn Fn(crate::permission::ActionRequest) -> Pin<Box<dyn Future<Output = Decision> + Send>>
        + Send
        + Sync,
>;

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
    /// 可选进度事件汇 (UI 流式渲染)。无则不发事件 (headless / 测试)。
    events: Option<EventSink>,
    /// 可选 Ask 升级钩子 (交互档审批): 策略审批器判 `Ask` 时 await 它拿人的裁决。无则 `Ask` fail-closed。
    ask_gate: Option<AskGate>,
    /// 可选持久化 (D2/D4)。无则纯内存。
    store: Option<SessionStore>,
    session_id: String,
    /// 自动压缩预算 (支柱 1): 决定 soft/hard 阈值。
    budget: Budget,
    /// 上一轮 server 回传的精确 prompt_tokens (用于「连窗口都太大」的摘要高水位判断)。
    last_prompt_tokens: Option<usize>,
    /// LLM 摘要顶档产物 (单条 user 消息): 一旦摘要过, 投影 = system 前缀 + 它 + 尾部。canonical 不动 (D1)。
    summary_msg: Option<Message>,
    /// 摘要边界 (canonical 下标): `[first_non_system .. boundary)` 已被 `summary_msg` 代表, 投影从此处起。
    compaction_boundary: Option<usize>,
    /// 摘要连续失败计数 (熔断): 达 `MAX_SUMMARIZE_FAILS` 后停止尝试摘要。
    summarize_fails: u32,
    /// 是否允许本 loop 派生子 agent (§5)。顶层 `true`; 派生出的子 agent 一律 `false` (深度 1, 防递归)。
    sub_agents_enabled: bool,
    /// 是否给 Bash spawn 的命令施加 OS 内核沙箱 (默认开, 安全优先; macOS Seatbelt 写收容, 其它平台 no-op)。
    /// [`with_sandbox(false)`](Self::with_sandbox) 关闭 (逃生口)。子 agent 继承父设置 (权限不放宽)。
    sandbox: bool,
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
            events: None,
            ask_gate: None,
            store: None,
            session_id: "default".to_string(),
            budget: Budget::default(),
            last_prompt_tokens: None,
            summary_msg: None,
            compaction_boundary: None,
            summarize_fails: 0,
            sub_agents_enabled: false,
            sandbox: true,
        }
    }

    /// 覆盖自动压缩预算 (按模型窗口尺寸调 LLM 摘要的高水位; 日常裁切走固定的 N-轮窗口, 与此无关)。
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// 启用/禁用子 agent 派生 (§5)。顶层入口 `true` 才允许; 派生出的子 agent 内部恒 `false` (深度 1)。
    pub fn with_sub_agents(mut self, on: bool) -> Self {
        self.sub_agents_enabled = on;
        self
    }

    /// 启用/禁用对 Bash spawn 命令的 OS 内核沙箱 (默认开)。关掉 = 命令可写工作区外 (逃生口, 如需改 ~ 下
    /// 非缓存文件的任务)。当前仅 macOS Seatbelt 真生效, 其它平台为 no-op。
    pub fn with_sandbox(mut self, on: bool) -> Self {
        self.sandbox = on;
        self
    }

    /// 构造一个子 agent 派生器: 每次调用跑一个**嵌套** `AgentLoop` 到收尾, 只返回其最终回答。
    /// 子 agent 继承父的 client / 工具集 / 审批器 / cap-std 写收容 / 共享 LSP+文件缓存 (权限**不放宽**),
    /// 但 `sub_agents_enabled=false` (深度 1, 防递归) 且 `events=None` (中间过程不进编排者 transcript)。
    fn make_spawner(&self) -> SubAgentSpawner {
        let client = self.client.clone();
        let tools = self.tools.clone();
        let approver = self.approver.clone();
        let files = self.files.clone();
        let lsp = self.lsp.clone();
        let fs = self.fs.clone();
        let cwd = self.cwd.clone();
        let budget = self.budget;
        let sandbox = self.sandbox;
        Arc::new(move |req: SubAgentRequest| {
            let client = client.clone();
            let tools = tools.clone();
            let approver = approver.clone();
            let files = files.clone();
            let lsp = lsp.clone();
            let fs = fs.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                // 子类型限权: explore/review = 只读子集 (无 write/exec); general/None = 全权。
                let tools = match req.agent_type.as_deref() {
                    Some("explore") | Some("review") => {
                        tools.subset(&["Read", "Grep", "Glob", "AstGrep", "Lsp"])
                    }
                    _ => tools,
                };
                let mut nested = AgentLoop::new(client, tools)
                    .with_approver(approver)
                    .with_fs_scope(fs)
                    .with_cwd(cwd.clone())
                    .with_budget(budget)
                    .with_sandbox(sandbox);
                // 共享父的 LSP 常驻服务器与文件缓存 (高效); sub_agents_enabled 保持 new() 默认 false。
                nested.files = files;
                nested.lsp = lsp;
                let sys = crate::prompt::sub_agent_prompt(&cwd, req.agent_type.as_deref());
                let mut session = Session::with_system(sys);
                session.push_user(format!("{}\n\n{}", req.description, req.prompt));
                nested.run_turn(&mut session).await.map_err(|e| e.to_string())?;
                Ok(last_assistant_text(&session))
            })
        })
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// 注入进度事件汇 (UI 流式渲染): 每段 assistant 文本 / 每次工具起止都会回调一次。
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.events = Some(sink);
        self
    }

    /// 注入 Ask 升级钩子 (交互档审批): 策略审批器判 `Ask` 时 agent `await` 它拿人的裁决 (Allow once)。
    /// 无此钩子时 `Ask` 维持 fail-closed 拒。
    pub fn with_ask_gate(mut self, gate: AskGate) -> Self {
        self.ask_gate = Some(gate);
        self
    }

    /// 发一个进度事件 (无汇则 no-op)。
    fn emit(&self, event: AgentEvent) {
        if let Some(sink) = &self.events {
            sink(event);
        }
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

    /// 清空持久化的当前会话 (New chat: 删掉该 session_id 的全部事件)。无 store 时 no-op。
    pub fn reset_store(&self) {
        if let Some(store) = &self.store {
            let _ = store.rollback(&self.session_id, -1); // 删 seq > -1 = 全部
        }
    }

    /// 组装一次请求: 从 canonical 投影出裁切后的 wire messages + 结构 normalize (③),
    /// 思考模式 + max 强度 (复杂 agent 场景, §7.1), 带全部工具定义。
    fn build_request(&self, session: &Session) -> ChatRequest {
        // 自动压缩 (支柱 1): 稳定的 N-轮 user-turn 滚动窗口投影 (只算 wire, canonical 不动)。
        // 有 LLM 摘要时: system 前缀 + 摘要 + 尾部窗口投影; 否则: 直接窗口投影。摘要事件在 `maybe_summarize`
        // 里发过, 此处不发 (routine 窗口化是每轮的常态, 静默)。
        let msgs = session.messages();
        let wire = match (&self.summary_msg, self.compaction_boundary) {
            (Some(summary), Some(boundary)) if boundary < msgs.len() => {
                assemble_with_summary(msgs, KEEP_RECENT_TURNS, summary, boundary)
            }
            _ => project_window(msgs, KEEP_RECENT_TURNS),
        };
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

    /// LLM 摘要顶档 (§1 阶段 4): 把旧前缀 `[system 前缀 .. 受保护尾部)` 压成一条结构化交接摘要 (user 消息)。
    /// 仅当结构阶梯都压不下去 (server 实测超 hard 阈值) 时由 `run_turn` 调。best-effort: 失败只计数+熔断,
    /// 绝不打断 turn。摘要进**投影侧** (`summary_msg` + `compaction_boundary`); canonical 全文 log 不动 (D1)。
    ///
    /// 用 user 角色注入 (无 reasoning_content → 天然规避 §7.4/§7.5 的 400, 且形成干净 user 边界)。
    /// 摘要请求本身: 非流式、thinking 关、无工具、专用摘要 system prompt。
    async fn maybe_summarize(&mut self, session: &Session) {
        if self.summarize_fails >= MAX_SUMMARIZE_FAILS {
            return; // 熔断: 连失太多次, 退回纯窗口投影
        }
        let msgs = session.messages();
        let Some(boundary) = protected_tail_start(msgs, KEEP_RECENT_TURNS) else {
            return; // user 轮不足以划出「旧前缀 + 受保护窗口」
        };
        let prefix_end = first_non_system_index(msgs);
        if boundary <= prefix_end {
            return; // 没有可摘要的旧前缀
        }
        let flat = flatten_for_summary(&msgs[prefix_end..boundary]);
        let req = ChatRequest {
            model: MODEL.to_string(),
            messages: vec![
                Message::system(crate::prompt::summarizer_prompt()),
                Message::user(format!("Earlier conversation to summarize:\n{flat}")),
            ],
            thinking: Some(Thinking { kind: ThinkingType::Disabled }),
            reasoning_effort: None,
            tools: None,
            tool_choice: None,
            max_tokens: Some(SUMMARY_MAX_TOKENS),
            temperature: None,
            stop: None,
            stream: false,
            response_format: None,
            stream_options: None,
        };
        let client = self.client.clone(); // 克隆 Arc 避免与下方 &mut self 借用冲突
        let text = match client.chat(&req).await {
            Ok(resp) => resp
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .filter(|c| !c.trim().is_empty()),
            Err(_) => None,
        };
        match text {
            Some(text) => {
                self.summary_msg = Some(Message::user(format!(
                    "[Compacted context — the earlier conversation was summarized to fit the context \
                     window. The full detail remains in the session log.]\n\n{text}"
                )));
                self.compaction_boundary = Some(boundary);
                self.summarize_fails = 0;
                self.emit(AgentEvent::Compacted {
                    rung: "summarize".to_string(),
                    before: 0,
                    after: 0,
                });
            }
            None => self.summarize_fails += 1,
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
        let mut resource_retries = 0u32;
        let mut summary_retries = 0u32;
        loop {
            let request = self.build_request(session);
            // 流式: 逐 chunk 把 assistant 文本 / reasoning 增量发给 UI (实时逐字 + token 边生成边涨)。
            // sink 是 self.events 的克隆 (独立 Arc, 不借 self), 故与 self.client 的方法借用不冲突。
            let sink = self.events.clone();
            let response = match self
                .client
                .chat_streaming(&request, |chunk: &ChatStreamChunk| {
                    if let Some(s) = &sink {
                        for sc in &chunk.choices {
                            if let Some(c) = &sc.delta.content {
                                if !c.is_empty() {
                                    s(AgentEvent::AssistantDelta(c.clone()));
                                }
                            }
                            if let Some(r) = &sc.delta.reasoning_content {
                                if !r.is_empty() {
                                    s(AgentEvent::ReasoningDelta(r.clone()));
                                }
                            }
                        }
                    }
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // 反应式兜底: 窗口投影仍被 DeepSeek 以 context 过长的 400 拒 (5 轮巨大) → 强制摘要后重发。
                    // 封顶 MAX_SUMMARY_RETRIES 防永久空转 (摘要可能因熔断/轮数不足 no-op)。
                    if is_context_length_error(&e) && summary_retries < MAX_SUMMARY_RETRIES {
                        summary_retries += 1;
                        self.maybe_summarize(session).await;
                        continue;
                    }
                    return Err(e);
                }
            };
            let usage = response.usage.clone();
            let choice = response
                .choices
                .into_iter()
                .next()
                .ok_or(syncode_llm::Error::EmptyResponse)?;
            let finish = choice.finish_reason;
            let message = choice.message;

            // 后端算力不足被中断 (§16): 不采纳本次输出, 退避后重发; 封顶防永久空转 (原 TODO)。
            if finish == Some(FinishReason::InsufficientSystemResource) {
                resource_retries += 1;
                if resource_retries > MAX_RESOURCE_RETRIES {
                    return Err(syncode_llm::Error::Api {
                        status: 503,
                        code: Some("insufficient_system_resource".to_string()),
                        message: format!(
                            "backend returned insufficient_system_resource {MAX_RESOURCE_RETRIES} times in a row"
                        ),
                        retry_after_secs: None,
                    });
                }
                let delay = syncode_llm::error::backoff_delay(resource_retries, None, 0.0);
                tokio::time::sleep(delay).await;
                continue;
            }

            // 采纳了本次输出 → 记录 server 精确 prompt_tokens + 报 token 用量。
            // 高水位判断: 连「窗口投影」都超过真实窗口的 summary_fraction (5 轮巨大/跨任务) → 点 LLM 摘要 (罕见)。
            let over_high_water = if let Some(u) = &usage {
                self.last_prompt_tokens = Some(u.prompt_tokens as usize);
                self.emit(AgentEvent::Usage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                    cache_hit_tokens: u.prompt_cache_hit_tokens,
                    reasoning_tokens: u
                        .completion_tokens_details
                        .as_ref()
                        .map(|d| d.reasoning_tokens)
                        .unwrap_or(0),
                });
                u.prompt_tokens as usize > self.budget.summary_high_water()
            } else {
                false
            };
            // 连窗口投影都超高水位 (5 轮巨大 / 跨任务) → 把旧 (已 stub) 前缀结构化重组成一段摘要 (罕见)。
            // canonical 全文 log 不动 (D1); 熔断后退回纯窗口投影。
            if over_high_water {
                self.maybe_summarize(session).await;
            }

            // assistant 全文 (含完整 reasoning_content) 回填 canonical。裁切只在发送投影侧做 (D1)。
            let tool_calls = message.tool_calls.clone().unwrap_or_default();
            let content = message.content.clone().unwrap_or_default();
            self.commit(session, message);
            // reasoning / 文本已在流式回调里逐字发出 (AssistantDelta/ReasoningDelta), 不再整段重发。

            if finish == Some(FinishReason::ToolCalls) && !tool_calls.is_empty() {
                // 一轮可有多个 tool_calls: **刻意串行**逐个执行并回填 (§11)。
                // 决策 (2026-06): 不并行。读类工具是 in-process 同步阻塞, join_all 在单 task 上不交错、
                // 不提速; 真并行需上线程池, 而 per-call 性能本就被 LLM 延迟淹没 (纲领: 不追 per-call 性能)。
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

            self.emit(AgentEvent::TurnDone);
            return Ok(()); // Stop / Length / ContentFilter → turn 结束
        }
    }

    /// 分发一次工具调用 (含进度事件): 包裹 [`dispatch_inner`](Self::dispatch_inner), 起止各发一个事件。
    async fn dispatch_tool(&self, tc: &ToolCall) -> String {
        let name = tc.function.name.clone();
        self.emit(AgentEvent::ToolStarted { name: name.clone(), args: tc.function.arguments.clone() });
        let result = self.dispatch_inner(tc).await;
        let is_error = result.starts_with("<tool_use_error>");
        self.emit(AgentEvent::ToolFinished { name, result: result.clone(), is_error });
        result
    }

    /// 分发一次工具调用, 返回回给模型的结果字符串。
    /// error message 写给模型读 (借鉴 CC `<tool_use_error>` 包裹, 利于自纠偏 §10)。
    async fn dispatch_inner(&self, tc: &ToolCall) -> String {
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
                    // Ask 升级 (§5.1): 装了交互钩子 → await 人的裁决 (Allow once); 无钩子 → fail-closed 拒。
                    // 钩子返回非 Allow (含人点 Deny / 窗口关 / 通道断 → 钩子兜底成 Deny) 一律不放行。
                    let granted = match &self.ask_gate {
                        Some(gate) => gate(req.clone()).await == Decision::Allow,
                        None => false,
                    };
                    if !granted {
                        let what = req.target.as_deref().unwrap_or(name);
                        return match self.ask_gate {
                            // 有交互通道但被拒 (人 Deny / 超时 / 关窗)。
                            Some(_) => format!(
                                "<tool_use_error>{name} needs human approval for a {:?} action ({what}); \
                                 the user did not approve it, so it was refused.</tool_use_error>",
                                req.class
                            ),
                            // 无交互通道 → fail-closed (headless / CLI)。
                            None => format!(
                                "<tool_use_error>{name} needs human approval for a {:?} action ({what}), but \
                                 interactive approval is not wired up here, so it was refused. If this is safe and \
                                 intended, pre-authorize it in the approver policy (e.g. an allowed write root).</tool_use_error>",
                                req.class
                            ),
                        };
                    }
                    // granted → 落到下方正常执行。
                }
            }
        }
        let mut ctx = ToolCtx::with_lsp(self.files.clone(), self.cwd.clone(), self.lsp.clone());
        ctx.fs = self.fs.clone();
        ctx.background = self.background.clone();
        ctx.sandbox = self.sandbox;
        // 启用 sub-agents 时注入派生器 (子 agent 内部不启用 → 其 ctx.sub_agent 为 None, 深度 1)。
        if self.sub_agents_enabled {
            ctx.sub_agent = Some(self.make_spawner());
        }
        match tool.call(args, &ctx).await {
            Ok(out) if out.is_error => format!("<tool_use_error>{}</tool_use_error>", out.content),
            Ok(out) => {
                // 编辑类工具带了 diff → 发 FileChanged 事件 (供 UI diff 视图)。diff 不进给模型的 content。
                if let Some(d) = &out.diff {
                    self.emit(AgentEvent::FileChanged {
                        path: d.path.clone(),
                        diff: d.unified.clone(),
                    });
                }
                // TodoWrite 带了清单 → 发 Todos 事件 (供 UI 渲染清单面板)。不进给模型的 content。
                if let Some(t) = &out.todos {
                    self.emit(AgentEvent::Todos(t.clone()));
                }
                out.content
            }
            Err(e) => format!("<tool_use_error>{e}</tool_use_error>"),
        }
    }
}

/// 取会话里最后一条非空 assistant 文本 (子 agent 的最终回答, 返给编排者)。
fn last_assistant_text(session: &Session) -> String {
    session
        .messages()
        .iter()
        .rev()
        .find(|m| {
            m.role == syncode_llm::wire::Role::Assistant
                && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
        })
        .and_then(|m| m.content.clone())
        .unwrap_or_else(|| "(sub-agent produced no final answer)".to_string())
}

/// 是否「context 过长」类的 400 (反应式压缩兜底用): estimate 低估漏过预算时, DeepSeek 直接以
/// 400 拒收过长 prompt。**保守**判定 (避免把别的 400 误当上下文超长): 仅 status==400 且 message
/// 明确提到 context/maximum length/too long。命中则强升一档压缩重试 (见 `run_turn`)。
fn is_context_length_error(e: &syncode_llm::Error) -> bool {
    if let syncode_llm::Error::Api { status: 400, message, .. } = e {
        let m = message.to_lowercase();
        m.contains("context length")
            || m.contains("maximum context")
            || m.contains("too long")
            || (m.contains("context") && m.contains("token"))
    } else {
        false
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
    async fn event_sink_emits_tool_started_and_finished() {
        use std::sync::Mutex;
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let agent = AgentLoop::new(dummy_client(), registry_with(Arc::new(EchoTool)))
            .with_event_sink(Arc::new(move |e| ev.lock().unwrap().push(e)));
        let out = agent.dispatch_tool(&call("echo", r#"{"text":"hi"}"#)).await;
        assert_eq!(out, "echo: hi");
        let evs = events.lock().unwrap();
        assert!(matches!(evs.first(), Some(AgentEvent::ToolStarted { .. })), "{evs:?}");
        assert!(
            matches!(evs.last(), Some(AgentEvent::ToolFinished { is_error: false, .. })),
            "{evs:?}"
        );
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
    async fn ask_gate_allow_lets_tool_execute() {
        // Ask + 交互钩子返回 Allow (人批本次) → 工具真执行。
        let (tool, ran) = class_tool(crate::permission::ActionClass::ArbitraryExec, None);
        let gate: AskGate = Arc::new(|_req| Box::pin(async { Decision::Allow }));
        let agent = AgentLoop::new(dummy_client(), registry_with(tool))
            .with_approver(Arc::new(AskAll))
            .with_ask_gate(gate);
        let out = agent.dispatch_tool(&call("danger", "{}")).await;
        assert_eq!(out, "ran", "human-approved Ask must execute");
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn ask_gate_deny_refuses_and_does_not_execute() {
        // Ask + 交互钩子返回 Deny (人拒 / 关窗 / 超时兜底) → 不执行, 报「未获批准」。
        let (tool, ran) = class_tool(crate::permission::ActionClass::ArbitraryExec, None);
        let gate: AskGate = Arc::new(|_req| Box::pin(async { Decision::Deny }));
        let agent = AgentLoop::new(dummy_client(), registry_with(tool))
            .with_approver(Arc::new(AskAll))
            .with_ask_gate(gate);
        let out = agent.dispatch_tool(&call("danger", "{}")).await;
        assert!(out.contains("did not approve"), "got: {out}");
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "denied Ask must NOT execute");
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
