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

use gpui::*;
use gpui_component::input::{Input as TextInput, InputEvent, InputState};
use gpui_component::scroll::{Scrollbar, ScrollbarShow};
use gpui_component::text::TextView;
use gpui_component::{button::*, *};
use syncode_core::permission::{ActionRequest, Decision, PolicyApprover};
use syncode_core::{AgentEvent, AgentLoop, AskGate, EventSink, FsScope, Session, ToolRegistry};
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

/// UI → worker 的控制消息。Task 跑一轮 (累积进常驻 session); Reset 丢弃 session 开新会话;
/// SetWorkspace 把 agent 的项目根 (cwd / 审批写根 / FsScope 收容根) 切到新目录并重建 + 开新会话。
enum WorkerMsg {
    Task(String),
    Reset,
    SetWorkspace(PathBuf),
}

/// agent 的 system prompt: 含当前项目根 + **工具选用策略** (引导优先用 in-process 语义工具, 别动不动
/// 退回 Bash)。工具的名字/描述/schema 已由 function-calling 自动发给模型, 故这里只给「何时用哪个」。
fn system_prompt(root: &Path) -> String {
    format!(
        "You are SynCode, an autonomous coding agent working in the project workspace at {}.\n\
         Prefer the dedicated in-process tools over the shell:\n\
         - Glob to list files or explore the directory tree (gitignore-aware); Read to read a file; \
         Grep for text search; AstGrep to search by code structure (syntax-aware, more precise than regex).\n\
         - Lsp for code intelligence (document symbols, go-to-definition, references) — use it to \
         locate definitions and implementors instead of grepping when you need symbols.\n\
         - Edit for exact text edits; AstEdit for structural rewrites (re-parsed, syntax-guaranteed).\n\
         - Use Bash only for builds, tests, git, and running programs.\n\
         Use absolute paths. Locate code with Grep/AstGrep/Lsp before editing; after editing, build \
         with cargo via Bash and fix any errors. Be concise.",
        root.display()
    )
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
    Status(String),
}

struct AgentApp {
    lines: Vec<Line>,
    input: Entity<InputState>,
    task_tx: smol::channel::Sender<WorkerMsg>,
    /// 当前 workspace (agent 操作的项目根); 顶栏展示、可经 Open folder 切换。
    workspace: PathBuf,
    /// 本会话累计 token 用量 (输入框上方展示)。
    usage: UsageTotals,
    /// 在途审批请求 (Some 时渲染审批卡片, 阻塞 agent 直到人点 Allow/Deny)。
    pending_approval: Option<ApprovalRequest>,
    running: bool,
    /// 变高虚拟列表状态 (只渲可见行, 内容多也不卡)。Bottom 对齐 = 聊天式贴底。
    /// 也直接当滚动条的 ScrollbarHandle 用 (位置/尺寸精确对应)。
    list_state: ListState,
    _drain: Task<()>,
    _appr_drain: Task<()>,
}

impl AgentApp {
    fn new(
        task_tx: smol::channel::Sender<WorkerMsg>,
        event_rx: smol::channel::Receiver<AgentEvent>,
        appr_rx: smol::channel::Receiver<ApprovalRequest>,
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
        let lines = vec![Line::Status("Ready — edit the task and press Enter (or Send).".into())];
        Self {
            // ListState 项数必须与 lines 同步 (初始 1 行)。Bottom 对齐 → 新内容贴底。
            list_state: ListState::new(lines.len(), ListAlignment::Bottom, px(800.)),
            lines,
            input,
            task_tx,
            // 初始 workspace = 进程 cwd, 与 worker 启动时取的根一致。
            workspace: std::env::current_dir().unwrap_or_default(),
            usage: UsageTotals::default(),
            pending_approval: None,
            running: false,
            _drain: drain,
            _appr_drain: appr_drain,
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
        match event {
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
                // 累加到会话总量 (不进 transcript, 只更新输入框上方的用量条)。
                self.usage.prompt += prompt_tokens;
                self.usage.completion += completion_tokens;
                self.usage.total += total_tokens;
                self.usage.cache_hit += cache_hit_tokens;
                self.usage.reasoning += reasoning_tokens;
            }
            AgentEvent::TurnDone => {
                self.push_line(Line::Status("— done —".into()));
                self.running = false;
            }
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

    /// 开新会话: 通知 worker 丢弃常驻 session, 清空本地 transcript。只在 idle 时可点 (render 已 disable),
    /// 故 worker 此刻阻塞于 recv, Reset 会被立刻处理, 不与在途事件流交错。
    fn new_chat(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let _ = self.task_tx.try_send(WorkerMsg::Reset);
        self.reset_lines(vec![Line::Status("New chat — context cleared.".into())]);
        self.usage = UsageTotals::default();
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
                    .child(div().flex_1().text_sm().text_color(summary_color).child(summary)),
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
            let rows: Vec<AnyElement> = text
                .lines()
                .map(|l| {
                    let color: Hsla = if l.starts_with("+++") || l.starts_with("---") {
                        theme.muted_foreground
                    } else if l.starts_with('@') {
                        color::blue() // hunk 头
                    } else if l.starts_with('+') {
                        color::green() // 增
                    } else if l.starts_with('-') {
                        color::red() // 删
                    } else {
                        theme.muted_foreground // 上下文
                    };
                    div().text_xs().text_color(color).child(l.to_string()).into_any_element()
                })
                .collect();
            col = col.child(
                v_flex()
                    .id(("body", i))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _ev, _window, cx| this.toggle_expanded(i, cx)))
                    .ml(px(20.))
                    .p_3()
                    .rounded(px(8.))
                    .bg(color::surface())
                    .border_1()
                    .border_color(theme.border)
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
                // transcript = 变高虚拟列表 (list, 只渲可见行 → 内容多也不卡) + 右侧 12px 滚动条槽。
                // 外层 flex_1+min_h(0) 给定有界视口高; list 自己处理滚动/滚轮/贴底; 滚动条读 list_state。
                .child(
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
                        ),
                )
                // 审批卡片 (仅在有在途 Ask 请求时): 阻塞中, 等人点 Allow once / Deny。
                .children(self.pending_approval.as_ref().map(|p| self.render_approval(p, cx)))
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
        div().px_1().pb_3().child(inner).into_any_element()
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

    /// 输入框上方的 token 用量条: 左侧 输入/输出, 右侧 缓存命中/思考/总量 (会话累计)。
    fn render_usage(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let u = &self.usage;
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
                    .child(usage_pill("↓", &fmt_tokens(u.completion), "out", color::blue(), theme)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(usage_pill("⚡", &fmt_tokens(u.cache_hit), "cached", theme.muted_foreground, theme))
                    .child(usage_pill("⊙", &fmt_tokens(u.reasoning), "think", color::amber(), theme))
                    .child(usage_pill("Σ", &fmt_tokens(u.total), "total", theme.foreground, theme)),
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
                    .child(
                        Button::new("send")
                            .primary()
                            .label(if running { "…" } else { "Send ↗" })
                            .disabled(running)
                            .on_click(cx.listener(|this, _ev, window, cx| this.submit(window, cx))),
                    ),
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

/// agent worker: 独立线程 + tokio runtime; 持一个**常驻 session** 跨任务累积上下文 (多轮)。
/// 收到 `Task` 就把它 push 进 session 跑一个 turn; 收到 `Reset` 丢弃 session 开新会话。
fn run_agent_worker(
    task_rx: smol::channel::Receiver<WorkerMsg>,
    event_tx: smol::channel::Sender<AgentEvent>,
    appr_tx: smol::channel::Sender<ApprovalRequest>,
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
        let client = Arc::new(client);
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
        // 给定根造一个 AgentLoop: cwd / 审批写根 / FsScope 收容根都钉在该根 (单一真相 #14)。
        // 切 workspace 时整体重建 (新根 = 新 PolicyApprover / FsScope / cwd; 工具缓存 / LSP 也重置)。
        let make_agent = |root: &Path| -> AgentLoop {
            let mut registry = ToolRegistry::new();
            register_builtins(&mut registry);
            AgentLoop::new(client.clone(), registry)
                .with_approver(Arc::new(PolicyApprover::new(root)))
                .with_fs_scope(Some(Arc::new(FsScope::new(root))))
                .with_cwd(root)
                .with_event_sink(sink.clone())
                .with_ask_gate(gate.clone())
        };

        let mut root = std::env::current_dir().unwrap_or_default();
        let mut agent = make_agent(&root);
        // 常驻 session: 跨任务累积 (多轮)。Reset / 切 workspace 时整体重建。
        let mut session = Session::with_system(system_prompt(&root));
        while let Ok(msg) = task_rx.recv().await {
            match msg {
                WorkerMsg::Reset => {
                    session = Session::with_system(system_prompt(&root));
                }
                WorkerMsg::SetWorkspace(path) => {
                    root = path;
                    agent = make_agent(&root); // 重建: 新根钉进两闸 + cwd
                    session = Session::with_system(system_prompt(&root));
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
        let (appr_tx, appr_rx) = smol::channel::unbounded::<ApprovalRequest>();

        // agent worker 独立线程 (自带 tokio runtime)。
        thread::spawn(move || run_agent_worker(task_rx, event_tx, appr_tx));

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
                let view = cx.new(|cx| AgentApp::new(task_tx, event_rx, appr_rx, window, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
