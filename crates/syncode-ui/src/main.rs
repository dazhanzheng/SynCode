//! SynCode UI — MVP 壳: 窗口 + 流式 transcript, 背后跑**真** agent loop。
//!
//! 架构 (方案 A): agent loop 跑在独立 tokio runtime 线程 (我们的栈是 tokio: reqwest/Bash 等);
//! gpui UI 跑在主线程。两者用 **smol channel** (运行时无关, gpui executor 与 tokio 都能 await) 通信:
//!   UI --task(String)--> worker;  worker --AgentEvent--> UI (经 AgentLoop 的 event sink)。
//! UI 侧用 `cx.spawn` 抽干事件流 → `weak.update` 改 view → `cx.notify` 重渲染 (照 stream_markdown 范式)。
//!
//! 本 MVP 用一个按钮跑**只读**演示任务 (不改仓库, 安全); 文本输入框留作下一步。

use std::sync::Arc;
use std::thread;

use gpui::*;
use gpui_component::input::{Input as TextInput, InputEvent, InputState};
use gpui_component::{button::*, *};
use syncode_core::permission::PolicyApprover;
use syncode_core::{AgentEvent, AgentLoop, EventSink, FsScope, Session, ToolRegistry};
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_tools::register_builtins;

/// 只读演示任务: 用 Read 看一个文件并总结 —— 流式好看、且不改任何东西。
const DEMO_TASK: &str = "Read the file crates/syncode-core/src/tool.rs and briefly summarize what \
    the `Tool` trait requires implementors to provide. Be concise.";

/// agent worker 的 system prompt (常驻 session 起头, 固定在前缀以吃 prompt cache, §12)。
const SYSTEM_PROMPT: &str = "You are SynCode, an autonomous coding agent in a Rust workspace. Use \
    absolute paths. Locate code with Grep/Read before answering. Be concise.";

/// UI → worker 的控制消息。Task 跑一轮 (累积进常驻 session); Reset 丢弃 session 开新会话。
enum WorkerMsg {
    Task(String),
    Reset,
}

/// transcript 一行。
#[derive(Clone)]
enum Line {
    User(String),
    Assistant(String),
    Tool { name: String, ok: bool, detail: String },
    Reasoning(usize),
    Status(String),
}

struct AgentApp {
    lines: Vec<Line>,
    input: Entity<InputState>,
    task_tx: smol::channel::Sender<WorkerMsg>,
    running: bool,
    scroll: ScrollHandle,
    _drain: Task<()>,
}

impl AgentApp {
    fn new(
        task_tx: smol::channel::Sender<WorkerMsg>,
        event_rx: smol::channel::Receiver<AgentEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 输入框: 预填演示任务 (可清空改写); 单行, Enter 提交。
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type a task for the agent… (Enter to run)")
                .default_value(DEMO_TASK)
        });
        // Enter → 提交 (subscribe_in 给 window, 才能清空输入框)。
        cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit(window, cx);
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
        Self {
            lines: vec![Line::Status("Ready — edit the task and press Enter (or Send).".into())],
            input,
            task_tx,
            running: false,
            scroll: ScrollHandle::new(),
            _drain: drain,
        }
    }

    fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantText(t) => self.lines.push(Line::Assistant(t)),
            AgentEvent::Reasoning { chars } => self.lines.push(Line::Reasoning(chars)),
            AgentEvent::ToolStarted { name, args } => {
                self.lines.push(Line::Tool { name, ok: true, detail: truncate(&args, 100) })
            }
            AgentEvent::ToolFinished { name, preview, is_error } => {
                self.lines.push(Line::Tool { name, ok: !is_error, detail: truncate(&preview, 160) })
            }
            AgentEvent::TurnDone => {
                self.lines.push(Line::Status("— done —".into()));
                self.running = false;
            }
        }
        self.scroll.scroll_to_bottom();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let task = self.input.read(cx).value().trim().to_string();
        if task.is_empty() {
            return;
        }
        self.lines.push(Line::User(task.clone()));
        self.running = true;
        let _ = self.task_tx.try_send(WorkerMsg::Task(task));
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// 开新会话: 通知 worker 丢弃常驻 session, 清空本地 transcript。只在 idle 时可点 (render 已 disable),
    /// 故 worker 此刻阻塞于 recv, Reset 会被立刻处理, 不与在途事件流交错。
    fn new_chat(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let _ = self.task_tx.try_send(WorkerMsg::Reset);
        self.lines = vec![Line::Status("New chat — context cleared.".into())];
        cx.notify();
    }

    fn render_line(&self, line: &Line, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (label, body, color) = match line {
            Line::User(t) => ("you", t.clone(), theme.primary),
            Line::Assistant(t) => ("syncode", t.clone(), theme.foreground),
            Line::Tool { name, ok, detail } => (
                if *ok { "tool" } else { "tool!" },
                format!("{name}  {detail}"),
                if *ok { theme.muted_foreground } else { theme.danger },
            ),
            Line::Reasoning(n) => ("think", format!("(reasoning: {n} chars)"), theme.muted_foreground),
            Line::Status(t) => ("·", t.clone(), theme.muted_foreground),
        };
        h_flex()
            .gap_2()
            .items_start()
            .child(div().w(px(64.)).flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(label))
            .child(div().flex_1().text_sm().text_color(color).child(body))
    }
}

impl Render for AgentApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let running = self.running;
        let rows: Vec<_> = self.lines.iter().map(|l| self.render_line(l, cx)).collect();

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(theme.background)
            // 标题栏: 标题 + (状态 + New chat)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_lg().font_bold().child("SynCode"))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(if running { "running…" } else { "idle" }),
                            )
                            .child(
                                Button::new("new-chat")
                                    .ghost()
                                    .label("New chat")
                                    .disabled(running)
                                    .on_click(
                                        cx.listener(|this, _ev, window, cx| this.new_chat(window, cx)),
                                    ),
                            ),
                    ),
            )
            // transcript (可滚动)
            .child(
                v_flex()
                    .id("transcript")
                    .flex_1()
                    .gap_2()
                    .p_3()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(6.))
                    .children(rows),
            )
            // 输入行: 文本框 + Send
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(TextInput::new(&self.input).flex_1())
                    .child(
                        Button::new("send")
                            .primary()
                            .label(if running { "…" } else { "Send" })
                            .disabled(running)
                            .on_click(cx.listener(|this, _ev, window, cx| this.submit(window, cx))),
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

/// agent worker: 独立线程 + tokio runtime; 持一个**常驻 session** 跨任务累积上下文 (多轮)。
/// 收到 `Task` 就把它 push 进 session 跑一个 turn; 收到 `Reset` 丢弃 session 开新会话。
fn run_agent_worker(
    task_rx: smol::channel::Receiver<WorkerMsg>,
    event_tx: smol::channel::Sender<AgentEvent>,
) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = event_tx.try_send(AgentEvent::AssistantText(format!("tokio runtime failed: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        let cfg = match DeepSeekConfig::from_env() {
            Ok(c) => c,
            Err(_) => {
                let _ = event_tx.try_send(AgentEvent::AssistantText(
                    "DEEPSEEK_API_KEY not set — load it before launching to run live.".into(),
                ));
                let _ = event_tx.try_send(AgentEvent::TurnDone);
                // 仍消费任务队列, 每个 Task 都报同样的提示 (Reset 静默忽略)。
                while let Ok(msg) = task_rx.recv().await {
                    if let WorkerMsg::Task(_) = msg {
                        let _ = event_tx.try_send(AgentEvent::AssistantText("(no API key)".into()));
                        let _ = event_tx.try_send(AgentEvent::TurnDone);
                    }
                }
                return;
            }
        };
        let client = match DeepSeekClient::new(cfg) {
            Ok(c) => c,
            Err(e) => {
                let _ = event_tx.try_send(AgentEvent::AssistantText(format!("client init failed: {e}")));
                return;
            }
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry);
        let root = std::env::current_dir().unwrap_or_default();
        let sink_tx = event_tx.clone();
        let sink: EventSink = Arc::new(move |e| {
            let _ = sink_tx.try_send(e);
        });
        let mut agent = AgentLoop::new(Arc::new(client), registry)
            .with_approver(Arc::new(PolicyApprover::new(&root)))
            .with_fs_scope(Some(Arc::new(FsScope::new(&root))))
            .with_cwd(&root)
            .with_event_sink(sink);

        // 常驻 session: 循环外建一次, 跨任务累积 (多轮上下文)。Reset 时整体重建。
        let mut session = Session::with_system(SYSTEM_PROMPT);
        while let Ok(msg) = task_rx.recv().await {
            match msg {
                WorkerMsg::Reset => {
                    session = Session::with_system(SYSTEM_PROMPT);
                }
                WorkerMsg::Task(task) => {
                    session.push_user(&task);
                    if let Err(e) = agent.run_turn(&mut session).await {
                        let _ = event_tx
                            .try_send(AgentEvent::AssistantText(format!("⚠ turn error: {e}")));
                        let _ = event_tx.try_send(AgentEvent::TurnDone);
                    }
                }
            }
        }
    });
}

fn main() {
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);

        let (task_tx, task_rx) = smol::channel::unbounded::<WorkerMsg>();
        let (event_tx, event_rx) = smol::channel::unbounded::<AgentEvent>();

        // agent worker 独立线程 (自带 tokio runtime)。
        thread::spawn(move || run_agent_worker(task_rx, event_tx));

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|cx| AgentApp::new(task_tx, event_rx, window, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
