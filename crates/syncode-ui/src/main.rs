//! SynCode UI — MVP 壳: 窗口 + 流式 transcript, 背后跑**真** agent loop。
//!
//! 架构 (方案 A): agent loop 跑在独立 tokio runtime 线程 (我们的栈是 tokio: reqwest/Bash 等);
//! gpui UI 跑在主线程。两者用 **smol channel** (运行时无关, gpui executor 与 tokio 都能 await) 通信:
//!   UI --task(String)--> worker;  worker --AgentEvent--> UI (经 AgentLoop 的 event sink)。
//! UI 侧用 `cx.spawn` 抽干事件流 → `weak.update` 改 view → `cx.notify` 重渲染 (照 stream_markdown 范式)。
//!
//! 本 MVP 用一个按钮跑**只读**演示任务 (不改仓库, 安全); 文本输入框留作下一步。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::*;
use gpui_component::input::{Input as TextInput, InputEvent, InputState};
use gpui_component::scroll::{Scrollbar, ScrollbarShow};
use gpui_component::text::TextView;
use gpui_component::{button::*, *};
use syncode_core::permission::{ActionRequest, Decision, PolicyApprover};
use syncode_core::state::{SessionMeta, SessionStore};
use syncode_core::{
    AgentEvent, AgentLoop, AskGate, EventSink, FsScope, Session, TodoItem, TodoStatus, ToolRegistry,
};
use syncode_llm::wire::{Message, Role};
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_tools::register_builtins;

// ═══════════════════════════════════════════════
//  Color palette — cohesive dark-theme accents
// ═══════════════════════════════════════════════
mod color {
    use gpui::*;
    /// Teal — assistant / tool-ok / brand accent
    pub fn teal() -> Hsla { rgb(0x4ec9b0).into() }
    /// Blue — diff hunk heads / interactive accent
    pub fn blue() -> Hsla { rgb(0x58a6ff).into() }
    /// Green — success / running / diff-add
    pub fn green() -> Hsla { rgb(0x3fb950).into() }
    /// Red — danger / error / diff-del
    pub fn red() -> Hsla { rgb(0xf85149).into() }
    /// Claude Code 暗色主题 diff 增行底色 (theme.ts 暗色档 diffAdded = rgb(34,92,43)) — 整行暗绿底
    pub fn diff_add_bg() -> Hsla { rgb(0x225c2b).into() }
    /// Claude Code 暗色主题 diff 删行底色 (diffRemoved = rgb(122,41,54)) — 整行暗红底
    pub fn diff_del_bg() -> Hsla { rgb(0x7a2936).into() }
    /// Amber — warning / approval / reasoning
    pub fn amber() -> Hsla { rgb(0xd29922).into() }
    /// Card background for user messages (subtle blue tint)
    pub fn user_card() -> Hsla { rgba(0x6675ff18).into() }
    /// Card border for user messages
    pub fn user_border() -> Hsla { rgba(0x6675ff30).into() }
    /// Subtle surface for expanded tool / reasoning / diff cards
    pub fn surface() -> Hsla { rgba(0x8a8a8a12).into() }
    /// Approval card background
    pub fn danger_bg() -> Hsla { rgba(0xf8514912).into() }
}

// ═══════════════════════════════════════════════
//  keystore — API key 的系统钥匙串持久化 (支柱 4: 机密不落明文盘)
// ═══════════════════════════════════════════════
/// DeepSeek API key 的安全存储: macOS Keychain / Windows Credential Manager 原生后端。
/// 设置面板写、worker 启动读 (仅当环境变量 `DEEPSEEK_API_KEY` 未设时)。其它平台无原生后端 → 回退「不支持」。
mod keystore {
    /// 钥匙串条目坐标: service = bundle id, account = 环境变量同名 (便于人在「钥匙串访问」里辨识)。
    const SERVICE: &str = "dev.syncode.app";
    const ACCOUNT: &str = "DEEPSEEK_API_KEY";

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn load() -> Option<String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
        match entry.get_password() {
            Ok(s) if !s.trim().is_empty() => Some(s),
            _ => None, // NoEntry / 空 / 读失败 → 视作未配置
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn save(key: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
        entry.set_password(key).map_err(|e| e.to_string())
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn clear() -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    // 非 macOS/Windows: 无原生钥匙串后端 → 回退「不支持」(仍可用环境变量启动)。
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn load() -> Option<String> { None }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn save(_key: &str) -> Result<(), String> {
        Err("secure key storage is not supported on this platform yet".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn clear() -> Result<(), String> { Ok(()) }
}

/// 粗略相对时间 ("just now" / "5m ago" / "3h ago" / "2d ago"), 供历史会话列表展示。
fn rel_time(updated_at_ms: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    let secs = (now - updated_at_ms).max(0) / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// UI → worker 的控制消息。Task 跑一轮 (累积进常驻 session); Reset 开新会话 (不删旧的);
/// SetWorkspace 把 agent 的项目根 (cwd / 审批写根 / FsScope 收容根) 切到新目录并重建 + 开新会话;
/// ListSessions 列当前 workspace 历史; SwitchSession 切到某条历史会话并 resume。
enum WorkerMsg {
    Task(String),
    Reset,
    SetWorkspace(PathBuf),
    /// 列出当前 workspace 的历史会话 (回信经 picker 通道 → UI 弹出选择器)。
    ListSessions,
    /// 切到某条历史会话并 resume (transcript 经 resume 通道重建)。
    SwitchSession(String),
    /// 设置面板提交的 DeepSeek API key: worker 存进 Keychain 并**重建 client/agent** (保留当前会话);
    /// 空串 = 清除已存 key。回信经 key 通道 (worker → UI) 报告新状态。
    SetApiKey(String),
}

/// API key 状态 (worker → UI): 驱动设置面板/警示横幅。`configured` = 此刻能否真的调用 DeepSeek;
/// `detail` = 人读说明 (来源 env / Keychain / 未配置 / 出错)。
struct KeyStatus {
    configured: bool,
    detail: String,
}

/// 生成 [`KeyStatus`] 的人读说明。`env_locked` = 启动时 `DEEPSEEK_API_KEY` 环境变量已设 (优先于 Keychain)。
fn key_detail(configured: bool, env_locked: bool) -> String {
    if env_locked {
        "Using DEEPSEEK_API_KEY from the environment (takes precedence over the Keychain).".into()
    } else if configured {
        "API key loaded from your macOS Keychain.".into()
    } else {
        "No API key set — paste your DeepSeek key below to save it to the Keychain.".into()
    }
}

/// agent 的 system prompt: 委托给 `syncode_core` 的**单一真相** builder (CLI/UI 同源, §2),
/// 避免两份发散。完整纲领 (doing-tasks / code-style / 工具纪律 / 可逆性 / 安全 / context 连续性 / tone)
/// 见 `syncode_core::prompt`。
fn system_prompt(root: &Path) -> String {
    syncode_core::system_prompt(root)
}

/// 交互审批请求 (worker → UI): 当策略审批器判 `Ask` 时, AskGate 把它发给 UI 并 await `reply`。
/// `reply` 收到 `Allow`/`Deny` 即解阻塞; 若 UI 关窗/丢弃 (reply Sender 随之 drop), worker 侧
/// `recv` 报错 → 兜底 `Deny` (fail-closed)。
struct ApprovalRequest {
    req: ActionRequest,
    reply: smol::channel::Sender<Decision>,
}

/// 累计 token 用量 (跨本会话所有 API 响应求和; New chat / 切 workspace 时清零)。
#[derive(Default, Clone, Copy)]
struct UsageTotals {
    prompt: u64,
    completion: u64,
    total: u64,
    cache_hit: u64,
    reasoning: u64,
}

/// transcript 一行。
#[derive(Clone)]
enum Line {
    User(String),
    Assistant(String),
    /// 一次工具调用 (一行, 可折叠): `args` 完整, `result` 在 finish 前为 None (运行中)。
    /// 折叠时显示 name + 结果摘要; 展开 (`expanded`) 显示完整 args + result。
    Tool { name: String, args: String, result: Option<String>, ok: bool, expanded: bool },
    /// 本轮推理 (CoT) 全文, 可折叠: 折叠显示摘要, 展开显示完整链。
    Reasoning { text: String, expanded: bool },
    /// 一次文件改动的 unified diff (可折叠, 默认展开): 按 +/-/@ 前缀逐行着色。
    Diff { path: String, text: String, expanded: bool },
    /// 任务清单面板 (TodoWrite): 整表替换、就地更新 (见 `cur_todos`)。
    Todos(Vec<TodoItem>),
    Status(String),
}

struct AgentApp {
    lines: Vec<Line>,
    input: Entity<InputState>,
    task_tx: smol::channel::Sender<WorkerMsg>,
    /// Stop 信号通道 (按 Stop 时 send 一个 () → worker 中止当前 turn)。
    cancel_tx: smol::channel::Sender<()>,
    /// 当前 workspace (agent 操作的项目根); 顶栏展示、可经 Open folder 切换。
    workspace: PathBuf,
    /// 本会话累计 token 用量 (输入框上方展示, 已采纳的精确值)。
    usage: UsageTotals,
    /// 流式: 当前正在追加的 assistant / reasoning 行下标 (delta 到了就 append 到它; 非 delta 事件清空)。
    cur_assistant: Option<usize>,
    cur_reasoning: Option<usize>,
    /// 任务清单面板所在行下标 (TodoWrite 整表替换时就地更新该行, 不重复 push)。
    cur_todos: Option<usize>,
    /// 流式: 本轮尚未被 Usage 精确值校准的「在途」字符数 (÷4 估成 token, 让 out/think 边生成边涨)。
    live_out_chars: usize,
    live_think_chars: usize,
    /// 在途审批请求 (Some 时渲染审批卡片, 阻塞 agent 直到人点 Allow/Deny)。
    pending_approval: Option<ApprovalRequest>,
    running: bool,
    /// 变高虚拟列表状态 (只渲可见行, 内容多也不卡)。Bottom 对齐 = 聊天式贴底。
    /// 也直接当滚动条的 ScrollbarHandle 用 (位置/尺寸精确对应)。
    list_state: ListState,
    _drain: Task<()>,
    _appr_drain: Task<()>,
    _resume_drain: Task<()>,
    /// 历史会话选择器: 在途列表 + 是否展开 (展开时主区域换成选择器)。
    sessions: Vec<SessionMeta>,
    picker_open: bool,
    _picker_drain: Task<()>,
    /// 设置面板: API key 输入框 (掩码), 是否展开 (展开时主区域换成设置面板)。
    key_input: Entity<InputState>,
    settings_open: bool,
    /// 当前 API key 是否就绪 + 人读状态 (worker 经 key 通道报告)。
    key_configured: bool,
    key_detail: String,
    /// 是否已收到过首个 key 状态 (首启没 key → 自动弹出设置面板, 仅一次)。
    key_status_seen: bool,
    _key_drain: Task<()>,
}

impl AgentApp {
    fn new(
        task_tx: smol::channel::Sender<WorkerMsg>,
        cancel_tx: smol::channel::Sender<()>,
        event_rx: smol::channel::Receiver<AgentEvent>,
        appr_rx: smol::channel::Receiver<ApprovalRequest>,
        resume_rx: smol::channel::Receiver<Vec<Message>>,
        picker_rx: smol::channel::Receiver<Vec<SessionMeta>>,
        key_rx: smol::channel::Receiver<KeyStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 输入框: 预填演示任务 (可清空改写); 单行, Enter 提交。
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type a task for the agent… (Enter to run)")
        });
        // Enter → 提交 (subscribe_in 给 window, 才能清空输入框)。
        cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit(window, cx);
            }
        })
        .detach();
        // 设置面板的 API key 输入框: 掩码 (••••), Enter 即保存。
        let key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("sk-… (paste your DeepSeek API key, Enter to save)")
        });
        cx.subscribe_in(&key_input, window, |this, _input, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.save_api_key(window, cx);
            }
        })
        .detach();

        // 抽干 agent 事件流 → 更新 view。
        let drain = cx.spawn(async move |weak, cx| {
            while let Ok(event) = event_rx.recv().await {
                let updated = weak.update(cx, |this, cx| {
                    this.apply(event);
                    cx.notify();
                });
                if updated.is_err() {
                    break; // view 没了 (窗口关闭) → 退出
                }
            }
        });
        // 抽干审批请求流 → 置 pending_approval (渲染审批卡片)。
        let appr_drain = cx.spawn(async move |weak, cx| {
            while let Ok(request) = appr_rx.recv().await {
                let updated = weak.update(cx, |this, cx| {
                    this.pending_approval = Some(request);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        // 抽干 resume 流 (worker 发来存档历史) → 重建 transcript (开机/切 workspace 自动接上次)。
        let resume_drain = cx.spawn(async move |weak, cx| {
            while let Ok(msgs) = resume_rx.recv().await {
                let updated = weak.update(cx, |this, cx| {
                    let mut lines = lines_from_messages(&msgs);
                    if lines.is_empty() {
                        lines.push(Line::Status(
                            "Ready — edit the task and press Enter (or Send).".into(),
                        ));
                    }
                    this.reset_lines(lines);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        // 抽干 picker 流 (worker 发来历史会话列表) → 弹出选择器。
        let picker_drain = cx.spawn(async move |weak, cx| {
            while let Ok(sessions) = picker_rx.recv().await {
                let updated = weak.update(cx, |this, cx| {
                    this.sessions = sessions;
                    this.picker_open = true;
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        // 抽干 key 状态流 (worker 报告 API key 是否就绪) → 更新横幅/面板; 首启没 key 自动弹设置。
        let key_drain = cx.spawn(async move |weak, cx| {
            while let Ok(status) = key_rx.recv().await {
                let updated = weak.update(cx, |this, cx| {
                    this.key_configured = status.configured;
                    this.key_detail = status.detail;
                    if !this.key_status_seen {
                        this.key_status_seen = true;
                        if !status.configured {
                            this.settings_open = true; // 首启缺 key → 直接打开设置, 引导填入
                        }
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let lines = vec![Line::Status("Ready — edit the task and press Enter (or Send).".into())];
        Self {
            // ListState 项数必须与 lines 同步 (初始 1 行)。Bottom 对齐 → 新内容贴底。
            list_state: ListState::new(lines.len(), ListAlignment::Bottom, px(800.)),
            lines,
            input,
            task_tx,
            cancel_tx,
            // 初始 workspace = 进程 cwd, 与 worker 启动时取的根一致。
            workspace: std::env::current_dir().unwrap_or_default(),
            usage: UsageTotals::default(),
            cur_assistant: None,
            cur_reasoning: None,
            cur_todos: None,
            live_out_chars: 0,
            live_think_chars: 0,
            pending_approval: None,
            running: false,
            _drain: drain,
            _appr_drain: appr_drain,
            _resume_drain: resume_drain,
            sessions: Vec::new(),
            picker_open: false,
            _picker_drain: picker_drain,
            key_input,
            settings_open: false,
            key_configured: false,
            key_detail: String::new(),
            key_status_seen: false,
            _key_drain: key_drain,
        }
    }

    /// 追加一行, 并同步 ListState (项数 +1)。
    fn push_line(&mut self, line: Line) {
        let n = self.lines.len();
        self.lines.push(line);
        self.list_state.splice(n..n, 1);
    }

    /// 重置 transcript 为给定行 (New chat / 切 workspace), 同步 ListState。
    fn reset_lines(&mut self, lines: Vec<Line>) {
        self.list_state.reset(lines.len());
        self.lines = lines;
        self.cur_todos = None; // 清单面板下标随 transcript 失效
    }

    /// 翻转第 `i` 行的展开状态 (Tool / Reasoning / Diff 通用)。header 和**展开后的卡片本身**都用它,
    /// 所以点击展开的卡片任意处就能折叠 (不必滚回头部)。高度变 → 重测该项。
    fn toggle_expanded(&mut self, i: usize, cx: &mut Context<Self>) {
        let toggled = match self.lines.get_mut(i) {
            Some(Line::Tool { expanded, .. })
            | Some(Line::Reasoning { expanded, .. })
            | Some(Line::Diff { expanded, .. }) => {
                *expanded = !*expanded;
                true
            }
            _ => false,
        };
        if toggled {
            self.list_state.remeasure_items(i..i + 1);
            cx.notify();
        }
    }

    /// 人对在途审批请求作答: 把决定回送给 worker (解阻塞 agent), 清掉卡片。
    fn resolve_approval(&mut self, decision: Decision, cx: &mut Context<Self>) {
        if let Some(p) = self.pending_approval.take() {
            let _ = p.reply.try_send(decision);
        }
        cx.notify();
    }

    fn apply(&mut self, event: AgentEvent) {
        // 非 delta 事件 = 当前这段流式文本/推理收口 → 折叠刚流完的 reasoning, 下个 delta 起新行。
        if !matches!(event, AgentEvent::AssistantDelta(_) | AgentEvent::ReasoningDelta(_)) {
            if let Some(i) = self.cur_reasoning {
                if let Some(Line::Reasoning { expanded, .. }) = self.lines.get_mut(i) {
                    *expanded = false; // 看完它「想」就收起来
                    self.list_state.remeasure_items(i..i + 1);
                }
            }
            self.cur_assistant = None;
            self.cur_reasoning = None;
        }
        match event {
            AgentEvent::AssistantDelta(t) => {
                self.live_out_chars += t.chars().count();
                self.append_stream(false, t);
            }
            AgentEvent::ReasoningDelta(t) => {
                // reasoning 也计入 completion(out), 同时单独计 think。
                self.live_out_chars += t.chars().count();
                self.live_think_chars += t.chars().count();
                self.append_stream(true, t);
            }
            AgentEvent::AssistantText(t) => self.push_line(Line::Assistant(t)),
            AgentEvent::Reasoning { text } => {
                self.push_line(Line::Reasoning { text, expanded: false })
            }
            AgentEvent::FileChanged { path, diff } => {
                // diff 是本功能的主角, 默认展开。
                self.push_line(Line::Diff { path, text: diff, expanded: true })
            }
            AgentEvent::ToolStarted { name, args } => {
                self.push_line(Line::Tool { name, args, result: None, ok: true, expanded: false })
            }
            AgentEvent::ToolFinished { result, is_error, .. } => {
                // dispatch 串行 (Started→Finished 严格配对): 回填最近一个未完成的 Tool 行;
                // 高度变了 → 只重测该项 (虚拟列表需要)。
                if let Some(idx) =
                    self.lines.iter().rposition(|l| matches!(l, Line::Tool { result: None, .. }))
                {
                    if let Line::Tool { result: slot, ok, .. } = &mut self.lines[idx] {
                        *slot = Some(result);
                        *ok = !is_error;
                    }
                    self.list_state.remeasure_items(idx..idx + 1);
                }
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
                cache_hit_tokens,
                reasoning_tokens,
            } => {
                // 精确值到 → 累加会话总量, 并清掉本轮在途估算 (out/think 从「估算」咬合成精确)。
                self.usage.prompt += prompt_tokens;
                self.usage.completion += completion_tokens;
                self.usage.total += total_tokens;
                self.usage.cache_hit += cache_hit_tokens;
                self.usage.reasoning += reasoning_tokens;
                self.live_out_chars = 0;
                self.live_think_chars = 0;
            }
            AgentEvent::Todos(todos) => {
                // 整表替换: 若已有清单面板行就**就地更新**, 否则新建一行并记下位置。
                match self.cur_todos {
                    Some(i) if matches!(self.lines.get(i), Some(Line::Todos(_))) => {
                        self.lines[i] = Line::Todos(todos);
                        self.list_state.remeasure_items(i..i + 1);
                    }
                    _ => {
                        let n = self.lines.len();
                        self.lines.push(Line::Todos(todos));
                        self.list_state.splice(n..n, 1);
                        self.cur_todos = Some(n);
                    }
                }
            }
            AgentEvent::Compacted { rung, before, after } => {
                // 自动 context 压缩触发 (支柱 1): 让用户看见「裁切」真的在发生 + 压了多少。
                let msg = if before > 0 {
                    format!("context compacted [{rung}]: ~{before} → ~{after} tok")
                } else {
                    format!("context compacted [{rung}]")
                };
                self.push_line(Line::Status(msg));
            }
            AgentEvent::TurnDone => {
                self.push_line(Line::Status("— done —".into()));
                self.running = false;
                self.live_out_chars = 0;
                self.live_think_chars = 0;
            }
            AgentEvent::Interrupted => {
                // 被 Stop 中止: 会话已被 worker 修复成可继续状态。清掉可能在途的审批卡。
                self.pending_approval = None;
                self.push_line(Line::Status("⏹ stopped — you can keep chatting".into()));
                self.running = false;
                self.live_out_chars = 0;
                self.live_think_chars = 0;
            }
        }
    }

    /// 流式追加一个增量: `reasoning` 决定接到当前 reasoning 行还是 assistant 行; 无当前行则新建。
    fn append_stream(&mut self, reasoning: bool, delta: String) {
        let cur = if reasoning { self.cur_reasoning } else { self.cur_assistant };
        if let Some(i) = cur {
            let appended = match self.lines.get_mut(i) {
                Some(Line::Assistant(s)) if !reasoning => {
                    s.push_str(&delta);
                    true
                }
                Some(Line::Reasoning { text, .. }) if reasoning => {
                    text.push_str(&delta);
                    true
                }
                _ => false,
            };
            if appended {
                self.list_state.remeasure_items(i..i + 1);
                return;
            }
        }
        // 无当前行 (或异常丢失) → 新建一行并记下下标。reasoning 流式时展开看它长。
        let i = self.lines.len();
        let line = if reasoning {
            Line::Reasoning { text: delta, expanded: true }
        } else {
            Line::Assistant(delta)
        };
        self.push_line(line);
        if reasoning {
            self.cur_reasoning = Some(i);
        } else {
            self.cur_assistant = Some(i);
        }
    }

    /// 把整个 transcript 序列化成纯文本 (供「Copy log」一键复制, 方便整段贴出去)。
    fn transcript_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::User(t) => out.push_str(&format!("## you\n{t}\n\n")),
                Line::Assistant(t) => out.push_str(&format!("## syncode\n{t}\n\n")),
                Line::Tool { name, args, result, ok, .. } => {
                    out.push_str(&format!("## tool: {name}{}\n", if *ok { "" } else { " (error)" }));
                    out.push_str(&format!("args: {args}\n"));
                    if let Some(r) = result {
                        out.push_str(&format!("result:\n{r}\n"));
                    }
                    out.push('\n');
                }
                Line::Reasoning { text, .. } => out.push_str(&format!("## reasoning\n{text}\n\n")),
                Line::Diff { path, text, .. } => {
                    out.push_str(&format!("## diff: {path}\n{text}\n\n"))
                }
                Line::Todos(items) => {
                    out.push_str("## plan\n");
                    for it in items {
                        let g = match it.status {
                            TodoStatus::Completed => "[x]",
                            TodoStatus::InProgress => "[~]",
                            TodoStatus::Pending => "[ ]",
                        };
                        out.push_str(&format!("{g} {}\n", it.content));
                    }
                    out.push('\n');
                }
                Line::Status(t) => out.push_str(&format!("· {t}\n\n")),
            }
        }
        out
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let task = self.input.read(cx).value().trim().to_string();
        if task.is_empty() {
            return;
        }
        self.push_line(Line::User(task.clone()));
        self.running = true;
        let _ = self.task_tx.try_send(WorkerMsg::Task(task));
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// 按 Stop: 通知 worker 中止当前 turn。worker 会修复会话 (使其合法、可继续) 并发回 Interrupted 事件,
    /// 由 [`apply`](Self::apply) 翻转 running + 落一条「stopped」行。
    fn stop(&mut self, _cx: &mut Context<Self>) {
        let _ = self.cancel_tx.try_send(());
    }

    /// 开新会话: 通知 worker 丢弃常驻 session, 清空本地 transcript。只在 idle 时可点 (render 已 disable),
    /// 故 worker 此刻阻塞于 recv, Reset 会被立刻处理, 不与在途事件流交错。
    fn new_chat(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let _ = self.task_tx.try_send(WorkerMsg::Reset);
        self.reset_lines(vec![Line::Status("New chat — started a fresh session.".into())]);
        self.usage = UsageTotals::default();
        self.settings_open = false;
        cx.notify();
    }

    /// 打开历史会话选择器: 让 worker 列出当前 workspace 的会话 (回信经 picker 通道 → 弹出)。
    fn open_session_picker(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let _ = self.task_tx.try_send(WorkerMsg::ListSessions);
        self.settings_open = false;
        cx.notify();
    }

    /// 选中一条历史会话: 通知 worker 切过去并 resume (transcript 经 resume 通道自动重建)。
    fn pick_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.picker_open = false;
        self.usage = UsageTotals::default();
        let _ = self.task_tx.try_send(WorkerMsg::SwitchSession(session_id));
        cx.notify();
    }

    /// 关闭历史会话选择器 (不切会话)。
    fn close_session_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = false;
        cx.notify();
    }

    /// 打开设置面板 (主区域换成设置), 顺手收起历史选择器。
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.picker_open = false;
        self.settings_open = true;
        cx.notify();
    }

    /// 关闭设置面板 (回到 transcript)。
    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    /// 保存设置面板里填的 API key: 发给 worker (存 Keychain + 重建 client/agent 用新 key)。空串 = 清除。
    /// **不在此处下结论**: 保存/清除的成败是异步的, 面板保持打开, 由 worker 经 key 通道回报的真实
    /// `key_detail` 在状态行如实反映 (避免「明明没存上却报已保存」)。用户看到结果后自行点 Close。
    fn save_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.key_input.read(cx).value().trim().to_string();
        let _ = self.task_tx.try_send(WorkerMsg::SetApiKey(key));
        // 清掉输入, 别把 key 以圆点形式留在框里。
        self.key_input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// 弹原生「选文件夹」对话框, 选中后把 workspace 切到该目录。idle 时才可点 (render 已 disable)。
    fn open_workspace(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose workspace folder".into()),
        });
        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = weak.update(cx, |this, cx| this.set_workspace(path, cx));
                }
            }
        })
        .detach();
    }

    /// 切 workspace: 通知 worker 重建 agent (新根 = cwd / 审批写根 / FsScope), 清空 transcript 开新会话。
    fn set_workspace(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let _ = self.task_tx.try_send(WorkerMsg::SetWorkspace(path.clone()));
        self.reset_lines(vec![Line::Status(format!(
            "Workspace → {} · context cleared.",
            path.display()
        ))]);
        self.workspace = path;
        self.usage = UsageTotals::default();
        cx.notify();
    }

    /// 普通消息行 (User / Assistant / Status): 角色 tag 在上、正文在下; User 套淡色卡片。
    /// Tool/Reasoning/Diff 有各自的可折叠渲染, 这里的分支仅作 `match` 兜底。
    fn render_line(&self, line: &Line, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        match line {
            Line::User(t) => self.msg_block("YOU", color::blue(), t, true, cx),
            Line::Assistant(t) => self.msg_block("SYNCODE", color::teal(), t, false, cx),
            // 状态行: 居中、暗、小 —— 当分隔提示用。
            Line::Status(t) => h_flex()
                .w_full()
                .justify_center()
                .py_1()
                .child(div().text_xs().text_color(theme.muted_foreground).child(t.clone()))
                .into_any_element(),
            Line::Tool { name, ok, result, .. } => self.msg_block(
                if *ok { "TOOL" } else { "TOOL!" },
                if *ok { color::teal() } else { color::red() },
                &format!("{name}  {}", result.as_deref().unwrap_or("…")),
                false,
                cx,
            ),
            Line::Reasoning { text, .. } => {
                self.msg_block("THINK", color::amber(), &truncate(text, 120), false, cx)
            }
            Line::Diff { path, .. } => {
                self.msg_block("DIFF", color::blue(), path, false, cx)
            }
            // 任务清单面板: PLAN tag + 逐项 (☑ 完成=暗 / ▶ 进行中=teal / ☐ 待办)。
            Line::Todos(items) => {
                let mut col = v_flex()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(div().text_xs().text_color(color::amber()).child("PLAN"));
                for it in items {
                    let (glyph, c) = match it.status {
                        TodoStatus::Completed => ("☑", theme.muted_foreground),
                        TodoStatus::InProgress => ("▶", color::teal()),
                        TodoStatus::Pending => ("☐", theme.foreground),
                    };
                    col = col.child(
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(div().text_sm().text_color(c).child(glyph))
                            .child(div().text_sm().text_color(c).child(it.content.clone())),
                    );
                }
                col.into_any_element()
            }
        }
    }

    /// 一个消息块: 顶部小号角色 tag + 下方正文。`carded` 时套淡色圆角卡片 (给 User 用, 视觉锚点)。
    fn msg_block(
        &self,
        tag: &str,
        tag_color: Hsla,
        body: &str,
        carded: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let mut block = v_flex()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .font_bold()
                    .text_color(tag_color)
                    .child(tag.to_string()),
            )
            .child(div().text_sm().text_color(theme.foreground).child(body.to_string()));
        if carded {
            block = block
                .p_3()
                .rounded(px(10.))
                .bg(color::user_card())
                .border_1()
                .border_color(color::user_border());
        }
        block.into_any_element()
    }

    /// assistant 回答: 角色 tag + **markdown 渲染** (标题/列表/粗体/行内码/代码块/链接), 可选中复制。
    /// 用 gpui-component 的 `TextView::markdown` (内建解析 + 高亮)。id 用行下标保持稳定。
    fn render_assistant(&self, i: usize, text: &str, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            
                            .font_bold()
                            .text_color(color::teal())
                            .child("SYNCODE"),
                    )
                    .child(copy_button(("copy-md", i), text.to_string(), cx)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(TextView::markdown(("assistant", i), text.to_string()).selectable(true)),
            )
    }

    /// 可折叠区块共用的 header: 可点击 (chevron + 角色 tag + 摘要), 圆角内边距, 翻转 `expanded`。
    fn collapsible_header(
        &self,
        kind: &'static str,
        i: usize,
        expanded: bool,
        tag: &str,
        tag_color: Hsla,
        summary: String,
        summary_color: Hsla,
        copy_text: String,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let glyph = if expanded { "▾" } else { "▸" };
        // 左侧可点击区 = 折叠开关; 右侧独立的 Copy 按钮 = 把本块完整内容写进剪贴板 (两动作分离, 不打架)。
        h_flex()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .id((kind, i))
                    .flex_1()
                    .min_w(px(0.)) // flex 子默认 min-width:auto → 不肯收缩到比内容窄; 置 0 摘要才能收缩
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(0xffffff0a)))
                    .on_click(cx.listener(move |this, _ev, _window, cx| this.toggle_expanded(i, cx)))
                    .child(
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(glyph),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            
                            .font_bold()
                            .text_color(tag_color)
                            .child(tag.to_string()),
                    )
                    // 摘要单行省略号 (truncate = overflow_hidden+nowrap+ellipsis) + 可收缩 (min_w 0):
                    // 否则长摘要撑爆整行、顶到滚动条, 文字还在右缘被硬切 (mid-letter)。
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_sm()
                            .text_color(summary_color)
                            .child(summary),
                    ),
            )
            .child(copy_button(("copy-block", i), copy_text, cx))
    }

    /// 可折叠工具行: 可点击 header (▸/▾ + name + 结果摘要); 展开时在下方显示完整 args + result。
    /// `i` = 该行在 `self.lines` 的下标, 点击经 `cx.listener` 翻转该行的 `expanded`。
    fn render_tool(
        &self,
        i: usize,
        name: &str,
        args: &str,
        result: Option<&str>,
        ok: bool,
        expanded: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // tag = 工具真名 (Read/Write/Edit/Glob/Grep/AstGrep/AstEdit/Lsp/Bash/BashOutput), 不再套 "TOOL"。
        // Bash 在 Windows 上默认跑 PowerShell, 故显示成 "PowerShell" 更贴实 (模型那边名字仍是 Bash)。
        let display_name = if name == "Bash" && cfg!(windows) { "PowerShell" } else { name };
        // ok=teal, error=red 表状态。
        let tag_color: Hsla = if ok { color::teal() } else { color::red() };
        let summary = match result {
            Some(r) => truncate(r, 120),
            None => "running…".to_string(),
        };

        let copy_text = match result {
            Some(r) => format!("$ {name} {args}\n{r}"),
            None => format!("$ {name} {args}"),
        };
        let header = self.collapsible_header(
            "tool",
            i,
            expanded,
            display_name,
            tag_color,
            summary,
            theme.muted_foreground,
            copy_text,
            cx,
        );

        let mut col = v_flex().gap_1().child(header);
        if expanded {
            // 卡片本身可点 → 点任意处折叠 (长结果不必滚回头部)。
            let mut card = v_flex()
                .id(("body", i))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _ev, _window, cx| this.toggle_expanded(i, cx)))
                .ml(px(20.))
                .gap_2()
                .p_3()
                .rounded(px(8.))
                .bg(color::surface())
                .border_1()
                .border_color(theme.border)
                .font_family(cx.theme().mono_font_family.clone())
                .text_xs()
                .child(
                    div()
                        .text_xs()
                        .font_bold()
                        .text_color(theme.muted_foreground)
                        .child("ARGS"),
                )
                .child(div().text_color(theme.muted_foreground).child(args.to_string()));
            if let Some(r) = result {
                card = card
                    .child(
                        div().w_full().h(px(1.)).bg(theme.border),
                    )
                    .child(
                        div()
                            .text_xs()
                            
                            .font_bold()
                            .text_color(theme.muted_foreground)
                            .child("RESULT"),
                    )
                    .child(div().text_color(theme.foreground).child(r.to_string()));
            }
            col = col.child(card);
        }
        col
    }

    /// 可折叠推理行: header (▸/▾ think + 摘要); 展开时下方显示完整 CoT。同 render_tool 的点击模型。
    fn render_reasoning(
        &self,
        i: usize,
        text: &str,
        expanded: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let summary = truncate(text, 120);

        let header = self.collapsible_header(
            "reasoning",
            i,
            expanded,
            "THINK",
            color::amber(),
            summary,
            theme.muted_foreground,
            text.to_string(),
            cx,
        );

        let mut col = v_flex().gap_1().child(header);
        if expanded {
            col = col.child(
                div()
                    .id(("body", i))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _ev, _window, cx| this.toggle_expanded(i, cx)))
                    .ml(px(20.))
                    .p_3()
                    .rounded(px(8.))
                    .bg(color::surface())
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(text.to_string()),
            );
        }
        col
    }

    /// 可折叠 diff 视图: header (▸/▾ diff + path + (+adds -dels)); 展开时按 unified diff 前缀逐行着色
    /// (绿=增 / 红=删 / 蓝=hunk头 / 暗=上下文), 等宽字体。语法 token 级高亮见路线图 §2.5 后续项。
    fn render_diff(
        &self,
        i: usize,
        path: &str,
        text: &str,
        expanded: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let adds = text.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
        let dels = text.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();

        let header = self.collapsible_header(
            "diff",
            i,
            expanded,
            "DIFF",
            color::blue(),
            format!("{path}   +{adds} -{dels}"),
            theme.foreground,
            text.to_string(),
            cx,
        );

        let mut col = v_flex().gap_1().child(header);
        if expanded {
            // Claude Code 暗色档配色: 增/删行**整行背景着色** (暗绿/暗红, 边到边), 文本走前景保证可读;
            // hunk 头蓝、文件头/上下文走 muted。比「只染 +/- 文字」更接近 CC 的 diff 观感。
            let rows: Vec<AnyElement> = text
                .lines()
                .map(|l| {
                    let (bg, fg): (Option<Hsla>, Hsla) =
                        if l.starts_with("+++") || l.starts_with("---") {
                            (None, theme.muted_foreground) // 文件头
                        } else if l.starts_with('@') {
                            (None, color::blue()) // hunk 头
                        } else if l.starts_with('+') {
                            (Some(color::diff_add_bg()), theme.foreground) // 增
                        } else if l.starts_with('-') {
                            (Some(color::diff_del_bg()), theme.foreground) // 删
                        } else {
                            (None, theme.muted_foreground) // 上下文
                        };
                    let mut row =
                        div().w_full().px_3().text_xs().text_color(fg).child(l.to_string());
                    if let Some(b) = bg {
                        row = row.bg(b);
                    }
                    row.into_any_element()
                })
                .collect();
            col = col.child(
                v_flex()
                    .id(("body", i))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _ev, _window, cx| this.toggle_expanded(i, cx)))
                    .ml(px(20.))
                    .py_2()
                    .rounded(px(8.))
                    .bg(color::surface())
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(rows),
            );
        }
        col
    }
}

impl Render for AgentApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let running = self.running;

        // 全屏背景, 内容居中收进一个最大宽度的阅读列 (宽窗也不至于撑满、留出舒适留白)。
        v_flex().size_full().bg(theme.background).items_center().child(
            v_flex()
                .size_full()
                .max_w(px(900.))
                .px_6()
                .py_4()
                .gap_3()
                .child(self.render_header(running, cx))
                // 主区域: picker 展开时换成历史会话选择器, 否则是 transcript。
                // transcript = 变高虚拟列表 (list, 只渲可见行 → 内容多也不卡) + 右侧 12px 滚动条槽。
                // 外层 flex_1+min_h(0) 给定有界视口高; list 自己处理滚动/滚轮/贴底; 滚动条读 list_state。
                .child(if self.settings_open {
                    self.render_settings(cx).into_any_element()
                } else if self.picker_open {
                    self.render_session_picker(cx).into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .child(
                            list(
                                self.list_state.clone(),
                                cx.processor(|this, ix, window, cx| {
                                    this.render_entry(ix, window, cx)
                                }),
                            )
                            .flex_1(),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_shrink_0()
                                .w(px(12.))
                                .h_full()
                                .child(
                                    Scrollbar::vertical(&self.list_state)
                                        .scrollbar_show(ScrollbarShow::Always),
                                ),
                        )
                        .into_any_element()
                })
                // 审批卡片 (仅在有在途 Ask 请求时): 阻塞中, 等人点 Allow once / Deny。
                .children(self.pending_approval.as_ref().map(|p| self.render_approval(p, cx)))
                // 缺 API key 警示横幅 (设置面板未开时): 一键打开设置。
                .children(
                    (!self.key_configured && !self.settings_open)
                        .then(|| self.render_key_banner(cx)),
                )
                .child(self.render_usage(cx))
                .child(self.render_input(running, cx)),
        )
    }
}

impl AgentApp {
    /// 渲染第 `ix` 行 (供虚拟 `list` 按需调用; 经 `cx.processor` 拿到 `&mut Self` + `Context<Self>`,
    /// 故点击/折叠的 `cx.listener` 照用)。每项底部留间距代替原来的 gap。
    fn render_entry(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(line) = self.lines.get(ix) else {
            return div().into_any_element();
        };
        // 工具调用 / 推理 / diff = 次要内容: 比正文 (user/assistant) **左右都更窄** (内缩),
        // 看着像嵌套的工具调用, 右侧也不再贴着滚动条。正文则用窄内边距、占满阅读宽度。
        let secondary =
            matches!(line, Line::Tool { .. } | Line::Reasoning { .. } | Line::Diff { .. });
        let inner = match line {
            Line::Tool { name, args, result, ok, expanded } => self
                .render_tool(ix, name, args, result.as_deref(), *ok, *expanded, cx)
                .into_any_element(),
            Line::Reasoning { text, expanded } => {
                self.render_reasoning(ix, text, *expanded, cx).into_any_element()
            }
            Line::Diff { path, text, expanded } => {
                self.render_diff(ix, path, text, *expanded, cx).into_any_element()
            }
            Line::Assistant(text) => self.render_assistant(ix, text, cx).into_any_element(),
            other => self.render_line(other, cx).into_any_element(),
        };
        // 工具类: 强制撑满列表宽 (w_full) 再 px_6 左右内缩 —— 否则 div 按内容自适应宽, padding 推不动右边界。
        // 正文: 保持内容自适应宽 (右侧自然留富余), 只给一点点内边距。
        let wrap = if secondary { div().w_full().px_6() } else { div().px_1() };
        wrap.pb_3().child(inner).into_any_element()
    }
}

impl AgentApp {
    /// 顶栏: 标题 + 副标题 / 状态点 + New chat。底部细分隔线。
    fn render_header(&self, running: bool, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dot: Hsla = if running { color::green() } else { theme.muted_foreground };
        let status = if running { "running" } else { "idle" };
        v_flex()
            .pb_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .size(px(28.))
                                    .rounded(px(8.))
                                    .bg(color::teal())
                                    .flex_shrink_0()
                                    .child(
                                        div()
                                            .size_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_sm()
                                            .text_color(rgb(0x000000))
                                            .child("S"),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_bold()
                                            .text_color(theme.foreground)
                                            .child("SynCode"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("📁 {}", short_path(&self.workspace))),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(12.))
                                    .bg(color::surface())
                                    .mr_1()
                                    .child(
                                        div()
                                            .size(px(7.))
                                            .rounded_full()
                                            .bg(dot)
                                            .border_1()
                                            .border_color(if running { color::green() } else { theme.border }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(status),
                                    ),
                            )
                            .child(
                                Button::new("copy-log")
                                    .ghost()
                                    .label("Copy log")
                                    .on_click(cx.listener(|this, _ev, _window, cx| {
                                        let text = this.transcript_text();
                                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                                    })),
                            )
                            .child(
                                Button::new("open-folder")
                                    .ghost()
                                    .label("Open folder…")
                                    .disabled(running)
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.open_workspace(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("settings")
                                    .ghost()
                                    .label("⚙ API key")
                                    .disabled(running)
                                    .on_click(cx.listener(|this, _ev, _window, cx| {
                                        this.open_settings(cx)
                                    })),
                            )
                            .child(
                                Button::new("history")
                                    .ghost()
                                    .label("History")
                                    .disabled(running)
                                    .on_click(cx.listener(|this, _ev, _window, cx| {
                                        this.open_session_picker(cx)
                                    })),
                            )
                            .child(
                                Button::new("new-chat")
                                    .ghost()
                                    .label("New chat")
                                    .disabled(running)
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.new_chat(window, cx)
                                    })),
                            ),
                    ),
            )
    }

    /// 历史会话选择器 (主区域内): 当前 workspace 的会话, 最近在前, 点一条即切过去 resume。
    fn render_session_picker(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        const MAX_ROWS: usize = 12;
        let total = self.sessions.len();
        let rows = self.sessions.iter().take(MAX_ROWS).enumerate().map(|(ix, m)| {
            let id = m.session_id.clone();
            let title = m
                .title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "(untitled session)".to_string());
            let when = rel_time(m.updated_at);
            div()
                .id(("session-row", ix))
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .px_3()
                .py_2()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(color::surface()))
                .on_click(
                    cx.listener(move |this, _ev, _window, cx| this.pick_session(id.clone(), cx)),
                )
                .child(div().text_color(theme.foreground).child(title))
                .child(div().flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(when))
                .into_any_element()
        });
        v_flex()
            .flex_1()
            .min_h(px(0.))
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().text_color(theme.foreground).child("History — this workspace"))
                    .child(
                        Button::new("close-picker").ghost().label("Close").on_click(
                            cx.listener(|this, _ev, _window, cx| this.close_session_picker(cx)),
                        ),
                    ),
            )
            .children((total == 0).then(|| {
                div()
                    .py_4()
                    .text_color(theme.muted_foreground)
                    .child("No past sessions in this workspace yet.")
            }))
            .children((total > 0).then(|| v_flex().gap_1().children(rows)))
            .children((total > MAX_ROWS).then(move || {
                div()
                    .pt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("+{} older", total - MAX_ROWS))
            }))
    }

    /// 输入框上方的 token 用量条: 左侧 输入/输出, 右侧 缓存命中/思考/总量 (会话累计)。
    fn render_usage(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let u = &self.usage;
        // 在途估算 (流式中尚未被精确 usage 校准的字符 ÷4)。让 out/think/total 边生成边涨。
        let live_out = (self.live_out_chars / 4) as u64;
        let live_think = (self.live_think_chars / 4) as u64;
        let out = u.completion + live_out;
        let think = u.reasoning + live_think;
        let total = u.total + live_out;
        h_flex()
            .justify_between()
            .items_center()
            .px_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .gap_2()
                    .child(usage_pill("↑", &fmt_tokens(u.prompt), "in", color::teal(), theme))
                    .child(usage_pill("↓", &fmt_tokens(out), "out", color::blue(), theme)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(usage_pill("⚡", &fmt_tokens(u.cache_hit), "cached", theme.muted_foreground, theme))
                    .child(usage_pill("⊙", &fmt_tokens(think), "think", color::amber(), theme))
                    .child(usage_pill("Σ", &fmt_tokens(total), "total", theme.foreground, theme)),
            )
    }

    /// 底部输入行: 文本框 + Send, 顶部细分隔线。
    fn render_input(&self, running: bool, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .pt_3()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .rounded(px(8.))
                            .bg(color::surface())
                            .border_1()
                            .border_color(theme.border)
                            .px_3()
                            .py_2()
                            .child(TextInput::new(&self.input).appearance(false)),
                    )
                    .child(if running {
                        // 跑动时变成 Stop: 按下中止当前 turn (worker 修复会话后可继续)。
                        Button::new("stop")
                            .danger()
                            .label("Stop ■")
                            .on_click(cx.listener(|this, _ev, _window, cx| this.stop(cx)))
                    } else {
                        Button::new("send")
                            .primary()
                            .label("Send ↗")
                            .on_click(cx.listener(|this, _ev, window, cx| this.submit(window, cx)))
                    }),
            )
    }

    /// 设置面板 (主区域内): 当场粘贴 DeepSeek API key → 存进 macOS Keychain → 立即生效。
    /// 复用与底部任务框同款的输入框外观 (掩码显示)。状态行实时反映来源 (env / Keychain / 未配置)。
    fn render_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let status_color = if self.key_configured { color::green() } else { color::amber() };
        v_flex()
            .flex_1()
            .min_h(px(0.))
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().text_color(theme.foreground).child("Settings — DeepSeek API key"))
                    .child(
                        Button::new("close-settings").ghost().label("Close").on_click(
                            cx.listener(|this, _ev, _window, cx| this.close_settings(cx)),
                        ),
                    ),
            )
            // 当前状态行 (绿=就绪 / 琥珀=缺失或注意)。
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(7.)).rounded_full().bg(status_color).flex_shrink_0())
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child(self.key_detail.clone()),
                    ),
            )
            // 安全说明。
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        "Stored securely in your macOS Keychain (service “dev.syncode.app”). \
                         The DEEPSEEK_API_KEY environment variable, if set at launch, takes precedence.",
                    ),
            )
            // 掩码输入框 (同底部任务框外观)。
            .child(
                div()
                    .rounded(px(8.))
                    .bg(color::surface())
                    .border_1()
                    .border_color(theme.border)
                    .px_3()
                    .py_2()
                    .child(TextInput::new(&self.key_input).appearance(false)),
            )
            // 动作: Save (主) + Clear (危险, 删除已存 key)。
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-key")
                            .primary()
                            .label("Save key")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.save_api_key(window, cx)
                            })),
                    )
                    .child(
                        Button::new("clear-key").ghost().label("Clear").on_click(cx.listener(
                            |this, _ev, window, cx| {
                                this.key_input.update(cx, |s, cx| s.set_value("", window, cx));
                                this.save_api_key(window, cx);
                            },
                        )),
                    ),
            )
    }

    /// 缺 API key 警示横幅: 红底琥珀框 + 「Set API key」按钮 (一键打开设置)。
    fn render_key_banner(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .justify_between()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(color::danger_bg())
            .border_1()
            .border_color(color::amber())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(theme.foreground)
                    .child("⚠ No API key — SynCode can't reach DeepSeek until you add your key."),
            )
            .child(
                Button::new("banner-set-key")
                    .primary()
                    .label("Set API key")
                    .on_click(cx.listener(|this, _ev, _window, cx| this.open_settings(cx))),
            )
    }
}

impl AgentApp {
    /// 审批卡片: 显示「Approve {class} action? ({target})」+ Allow once / Deny。点击经
    /// [`resolve_approval`](Self::resolve_approval) 把决定回送 worker (解阻塞 agent)。
    fn render_approval(&self, p: &ApprovalRequest, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let what = p.req.target.clone().unwrap_or_else(|| p.req.tool.clone());
        let class = format!("{:?}", p.req.class);
        v_flex()
            .gap_3()
            .p_4()
            .border_1()
            .border_color(color::amber())
            .bg(color::danger_bg())
            .rounded(px(10.))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            
                            .font_bold()
                            .text_color(color::amber())
                            .child(format!("⚠ APPROVAL NEEDED · {class}")),
                    )
                    .child(div().text_sm().text_color(theme.foreground).child(what)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("appr-allow")
                            .primary()
                            .label("Allow once")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.resolve_approval(Decision::Allow, cx)
                            })),
                    )
                    .child(
                        Button::new("appr-deny")
                            .danger()
                            .label("Deny")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.resolve_approval(Decision::Deny, cx)
                            })),
                    ),
            )
    }
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim().replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// 小号「Copy」按钮: 点击把 `text` 写进系统剪贴板。用可点击 div (比 Button 紧凑、好控样式)。
/// 解决 gpui 里普通文本不可拖选的问题 —— 整段一键复制, 方便把工具输出/报错贴出去。
fn copy_button(id: impl Into<ElementId>, text: String, cx: &Context<AgentApp>) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .id(id)
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded(px(5.))
        .text_xs()
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(|s| s.bg(rgba(0xffffff0a)).text_color(theme.foreground))
        .child("Copy")
        .on_click(cx.listener(move |_this, _ev, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        }))
}

/// token 用量小药丸: 符号 + 数值 + 标签, 用给定颜色。
fn usage_pill(icon: &str, val: &str, label: &str, accent: Hsla, theme: &Theme) -> impl IntoElement {
    h_flex()
        .gap_1()
        .items_center()
        .px_2()
        .py_1()
        .rounded(px(4.))
        .child(div().text_color(accent).child(icon.to_string()))
        .child(div().text_color(theme.foreground).child(val.to_string()))
        .child(div().text_color(theme.muted_foreground).child(label.to_string()))
}

/// token 数人读化: <1k 原样, <1M → x.xk, 否则 x.xxM。
fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    }
}

/// 路径短展示: 太长时保留**尾部** (更有信息量), 前面用 `…` 省略。
fn short_path(p: &Path) -> String {
    let s = p.display().to_string();
    let n = s.chars().count();
    const MAX: usize = 56;
    if n <= MAX {
        s
    } else {
        let tail: String = s.chars().skip(n - (MAX - 1)).collect();
        format!("…{tail}")
    }
}

/// 用户级数据目录 (放持久化 DB)。Windows `%LOCALAPPDATA%`; 其它 `$XDG_DATA_HOME` / `~/.local/share`。
fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
        })
    }
}

/// 会话持久化 DB 路径 `<data_dir>/SynCode/sessions.db` (建好父目录)。取不到目录则 None → 降级纯内存。
fn session_db_path() -> Option<PathBuf> {
    let base = data_dir()?.join("SynCode");
    std::fs::create_dir_all(&base).ok()?;
    Some(base.join("sessions.db"))
}

/// resume 时把存档的 canonical messages 重建成 UI transcript 行 (近似还原: diff 卡不持久化, 故不重现)。
fn lines_from_messages(msgs: &[Message]) -> Vec<Line> {
    let mut lines = Vec::new();
    for m in msgs {
        match m.role {
            Role::System => {} // 系统提示不显示
            Role::User => {
                if let Some(c) = &m.content {
                    if c == "[Request interrupted by user]" {
                        lines.push(Line::Status("⏹ (interrupted earlier)".into()));
                    } else if !c.trim().is_empty() {
                        lines.push(Line::User(c.clone()));
                    }
                }
            }
            Role::Assistant => {
                if let Some(r) = &m.reasoning_content {
                    if !r.trim().is_empty() {
                        lines.push(Line::Reasoning { text: r.clone(), expanded: false });
                    }
                }
                if let Some(c) = &m.content {
                    if !c.trim().is_empty() {
                        lines.push(Line::Assistant(c.clone()));
                    }
                }
                for tc in m.tool_calls.iter().flatten() {
                    lines.push(Line::Tool {
                        name: tc.function.name.clone(),
                        args: tc.function.arguments.clone(),
                        result: None,
                        ok: true,
                        expanded: false,
                    });
                }
            }
            Role::Tool => {
                // 回填最近一个未完成的 Tool 行 (与实时 apply 同逻辑)。
                if let Some(c) = &m.content {
                    let is_err = c.starts_with("<tool_use_error>");
                    if let Some(Line::Tool { result, ok, .. }) =
                        lines.iter_mut().rev().find(|l| matches!(l, Line::Tool { result: None, .. }))
                    {
                        *result = Some(c.clone());
                        *ok = !is_err;
                    }
                }
            }
        }
    }
    lines
}

/// agent worker: 独立线程 + tokio runtime; 持一个**常驻 session** 跨任务累积上下文 (多轮)。
/// 收到 `Task` 就把它 push 进 session 跑一个 turn; 收到 `Reset` 丢弃 session 开新会话。
fn run_agent_worker(
    task_rx: smol::channel::Receiver<WorkerMsg>,
    event_tx: smol::channel::Sender<AgentEvent>,
    appr_tx: smol::channel::Sender<ApprovalRequest>,
    cancel_rx: smol::channel::Receiver<()>,
    resume_tx: smol::channel::Sender<Vec<Message>>,
    picker_tx: smol::channel::Sender<Vec<SessionMeta>>,
    key_tx: smol::channel::Sender<KeyStatus>,
) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = event_tx.try_send(AgentEvent::AssistantText(format!("tokio runtime failed: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        // event sink + Ask 升级钩子: 不依赖 API key, 一次建好 (跨 client 重建复用)。
        let sink_tx = event_tx.clone();
        let sink: EventSink = Arc::new(move |e| {
            let _ = sink_tx.try_send(e);
        });
        // Ask 升级钩子: 把审批请求发给 UI + await 回信。发送失败 / 通道断 → 兜底 Deny (fail-closed)。
        let gate: AskGate = Arc::new(move |req: ActionRequest| {
            let appr_tx = appr_tx.clone();
            let fut: Pin<Box<dyn Future<Output = Decision> + Send>> = Box::pin(async move {
                let (reply_tx, reply_rx) = smol::channel::bounded::<Decision>(1);
                if appr_tx.try_send(ApprovalRequest { req, reply: reply_tx }).is_err() {
                    return Decision::Deny; // UI 没了
                }
                reply_rx.recv().await.unwrap_or(Decision::Deny) // 通道断 / 关窗
            });
            fut
        });
        // 会话持久化 DB (按 workspace 一份; session_id = 规范化路径)。取不到目录则降级纯内存。
        let db_path = session_db_path();
        // 给定 client + 根造一个 AgentLoop: cwd / 审批写根 / FsScope 收容根都钉在该根 (单一真相 #14)。
        // 切 workspace 或换 API key 时整体重建 (新 client / 新 PolicyApprover / FsScope / cwd)。
        // 挂持久化: 每个 workspace 一份历史 (开机/切换自动 resume)。
        let make_agent = |client: Arc<DeepSeekClient>, root: &Path| -> AgentLoop {
            let mut registry = ToolRegistry::new();
            register_builtins(&mut registry);
            let mut agent = AgentLoop::new(client, registry)
                .with_approver(Arc::new(PolicyApprover::new(root)))
                .with_fs_scope(Some(Arc::new(FsScope::new(root))))
                .with_cwd(root)
                .with_sub_agents(true) // 顶层启用子 agent 派生 (深度 1)
                .with_event_sink(sink.clone())
                .with_ask_gate(gate.clone());
            if let Some(db) = &db_path {
                if let Ok(store) = SessionStore::open(db) {
                    agent = agent.with_store(store, root.to_string_lossy().to_string());
                }
            }
            agent
        };

        // 从 store 取回该 workspace 的历史 (空则新建带 system prompt 的会话); 再把存档行发给 UI 重建界面。
        let resume = |agent: &AgentLoop, root: &Path| -> Session {
            let s = agent.resume_session();
            if s.messages().is_empty() {
                Session::with_system(system_prompt(root))
            } else {
                s
            }
        };

        // API key 解析: 环境变量优先 (终端/harness 行为不变), 否则读 Keychain。env 已设 = 后续仍以它为准。
        let env_key = std::env::var("DEEPSEEK_API_KEY").ok().filter(|s| !s.trim().is_empty());
        let env_locked = env_key.is_some();
        let initial_key = env_key.or_else(keystore::load);

        let mut root = std::env::current_dir().unwrap_or_default();
        // client / agent / session 现在都是 Option: 没 key 时为 None, 用户在设置里填入后再建。
        let mut client: Option<Arc<DeepSeekClient>> = initial_key
            .and_then(|k| DeepSeekClient::new(DeepSeekConfig::new(k)).ok().map(Arc::new));
        let mut agent: Option<AgentLoop> = client.clone().map(|c| make_agent(c, &root));
        let mut session: Option<Session> = agent.as_ref().map(|a| resume(a, &root));
        if let Some(s) = &session {
            let _ = resume_tx.try_send(s.messages().to_vec());
        }
        // 首报状态: UI 据此决定是否弹设置面板 / 显示横幅。
        let _ = key_tx.try_send(KeyStatus {
            configured: client.is_some(),
            detail: key_detail(client.is_some(), env_locked),
        });

        while let Ok(msg) = task_rx.recv().await {
            match msg {
                WorkerMsg::SetApiKey(key) => {
                    let key = key.trim().to_string();
                    if key.is_empty() {
                        // 空 = 清除已存 key。删除成功才把内存置 None (回到「未配置」, 在途会话丢弃);
                        // 删除失败则保持现状并如实报错 —— 别让内存说「没 key」而钥匙串里还留着 (下次启动又冒出来)。
                        match keystore::clear() {
                            Ok(()) => {
                                client = None;
                                agent = None;
                                session = None;
                                let _ = key_tx.try_send(KeyStatus {
                                    configured: false,
                                    detail: key_detail(false, env_locked),
                                });
                            }
                            Err(e) => {
                                let _ = key_tx.try_send(KeyStatus {
                                    configured: client.is_some(),
                                    detail: format!("Couldn't remove the key from the Keychain: {e}"),
                                });
                            }
                        }
                        continue;
                    }
                    match DeepSeekClient::new(DeepSeekConfig::new(key.clone())) {
                        Ok(c) => {
                            // 存进钥匙串 (失败也不阻断本次会话, 但要如实回报 —— 不谎称已保存)。
                            let saved = keystore::save(&key);
                            let c = Arc::new(c);
                            let was_none = agent.is_none();
                            // 换 key 前记下当前会话 id: make_agent 重建会把 store 指针拨到「最近一条」,
                            // 必须拨回用户此刻所在会话, 否则后续 run_turn 落库串到别的会话。
                            let prev_sid = agent.as_ref().map(|a| a.current_session_id().to_string());
                            let mut a = make_agent(c.clone(), &root);
                            client = Some(c);
                            if was_none {
                                // 首次设 key: 载入该 workspace 历史并让 UI 重建 transcript。
                                let s = resume(&a, &root);
                                let _ = resume_tx.try_send(s.messages().to_vec());
                                session = Some(s);
                            } else if let Some(sid) = prev_sid {
                                // 换 key: 仅把新 agent 拨回当前会话; 内存 session 与 UI transcript 原样保留。
                                a.switch_session(sid);
                            }
                            agent = Some(a);
                            // 如实状态: 保存失败 / (env 下次启动仍会覆盖) / 已保存。client 现已用新 key 运行。
                            let detail = match &saved {
                                Err(e) => format!(
                                    "Key active for this session, but saving to the Keychain failed: {e}"
                                ),
                                Ok(()) if env_locked =>
                                    "API key saved to the Keychain and active now. Note: the DEEPSEEK_API_KEY \
                                     environment variable will take precedence again on the next launch."
                                        .into(),
                                Ok(()) => "API key saved to your macOS Keychain.".into(),
                            };
                            let _ = key_tx.try_send(KeyStatus { configured: true, detail });
                        }
                        Err(e) => {
                            let _ = key_tx.try_send(KeyStatus {
                                configured: client.is_some(),
                                detail: format!("client init failed: {e}"),
                            });
                        }
                    }
                }
                WorkerMsg::Reset => {
                    // New chat: 新开一条会话 (不删旧的; 旧会话进历史可回看)。没 key 时无操作。
                    if let Some(a) = agent.as_mut() {
                        a.start_new_session();
                        session = Some(Session::with_system(system_prompt(&root)));
                    }
                }
                WorkerMsg::ListSessions => {
                    if let Some(a) = agent.as_ref() {
                        let _ = picker_tx.try_send(a.sessions());
                    }
                }
                WorkerMsg::SwitchSession(id) => {
                    if let Some(a) = agent.as_mut() {
                        a.switch_session(id);
                        let s = resume(a, &root); // 载入该会话历史
                        let _ = resume_tx.try_send(s.messages().to_vec()); // UI 重建 transcript
                        session = Some(s);
                    }
                }
                WorkerMsg::SetWorkspace(path) => {
                    root = path;
                    // 有 client 才重建 agent; 没 key 时仅记下新根 (填 key 时用它建)。
                    if let Some(c) = client.clone() {
                        let a = make_agent(c, &root); // 新根钉进两闸 + cwd + 该 workspace 的 store
                        let s = resume(&a, &root); // 载入新 workspace 的历史
                        let _ = resume_tx.try_send(s.messages().to_vec()); // 让 UI 重建 transcript
                        agent = Some(a);
                        session = Some(s);
                    }
                }
                WorkerMsg::Task(task) => match (agent.as_mut(), session.as_mut()) {
                    (Some(agent), Some(session)) => {
                        session.push_user(&task);
                        // 清掉本轮开始前残留的 cancel 信号 (上一次 Stop 的余波别误杀新 turn)。
                        while cancel_rx.try_recv().is_ok() {}
                        // run_turn 跑这一轮; 同时盯着 cancel —— Stop 触发 → 丢弃 run_turn future
                        // (在途 HTTP 请求随之中止)。cancel 分支体不碰 agent/session, 借用在 select! 结束即释放。
                        let cancelled = tokio::select! {
                            r = agent.run_turn(session) => {
                                if let Err(e) = r {
                                    let _ = event_tx
                                        .try_send(AgentEvent::AssistantText(format!("⚠ turn error: {e}")));
                                    let _ = event_tx.try_send(AgentEvent::TurnDone);
                                }
                                false
                            }
                            _ = cancel_rx.recv() => true,
                        };
                        if cancelled {
                            // 修复半截 turn 使会话合法、可继续 (悬空 tool_call 补「中断」结果 + 中断标记)。
                            session.repair_after_interrupt();
                            let _ = event_tx.try_send(AgentEvent::Interrupted);
                        }
                    }
                    _ => {
                        // 没 key: 提示去设置里填 (而不是静默失败)。
                        let _ = event_tx.try_send(AgentEvent::AssistantText(
                            "No API key configured — open ⚙ API key and paste your DeepSeek key."
                                .into(),
                        ));
                        let _ = event_tx.try_send(AgentEvent::TurnDone);
                    }
                },
            }
        }
    });
}

fn main() {
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);

        let (task_tx, task_rx) = smol::channel::unbounded::<WorkerMsg>();
        let (event_tx, event_rx) = smol::channel::unbounded::<AgentEvent>();
        let (appr_tx, appr_rx) = smol::channel::unbounded::<ApprovalRequest>();
        // Stop 信号 (UI → worker): 与 task 分开, 因为 worker 跑 turn 时阻塞在 run_turn、不在 recv task。
        let (cancel_tx, cancel_rx) = smol::channel::unbounded::<()>();
        // resume 通道 (worker → UI): 开机/切 workspace 时把存档历史发给 UI 重建 transcript。
        let (resume_tx, resume_rx) = smol::channel::unbounded::<Vec<Message>>();
        // picker 通道 (worker → UI): History 按钮请求时, worker 把会话列表发回 → 弹出选择器。
        let (picker_tx, picker_rx) = smol::channel::unbounded::<Vec<SessionMeta>>();
        // key 状态通道 (worker → UI): 启动/保存后报告 API key 是否就绪 → UI 弹设置面板 / 显示横幅。
        let (key_tx, key_rx) = smol::channel::unbounded::<KeyStatus>();

        // agent worker 独立线程 (自带 tokio runtime)。
        thread::spawn(move || {
            run_agent_worker(task_rx, event_tx, appr_tx, cancel_rx, resume_tx, picker_tx, key_tx)
        });

        cx.spawn(async move |cx| {
            let bounds = Bounds {
                origin: point(px(140.), px(90.)),
                size: size(px(1040.), px(740.)),
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("SynCode".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| {
                    AgentApp::new(
                        task_tx, cancel_tx, event_rx, appr_rx, resume_rx, picker_rx, key_rx, window,
                        cx,
                    )
                });
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
