//! Bash: 跑 shell 命令 (架构 §6.0 机制① 万能逃生口, §10)。
//!
//! 让 agent 能跑 build/test/git —— 补上「写→跑→看失败→改」验证闭环里缺的那环。
//! 子进程塞进 [`ProcessContainer`](syncode_sandbox::ProcessContainer): Windows 上是 Job Object,
//! **超时/取消时杀整棵进程树**(`cargo test` 会 spawn `rustc`/test 子进程, 朴素 kill 会留孤儿)。
//!
//! **审批 (§7.5)**: 每条命令经 [`classify_command`] 按可逆性 / 影响面定级 (中等语义档) 再过审批闸。
//! Build / test / 本地 git / 读 → 默认放行; push / 装包 / 联网 / 删项目外 / 提权 → `Ask`; 未识别 →
//! `ArbitraryExec` (fail-closed Ask)。**此分类是 UX/策略层启发式, 不是安全边界** —— 真边界靠沙箱
//! (见 [`syncode_core::permission`] 顶注); shell 能 `;`/`$(...)`/别名绕过解析, 故只能偏保守。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use syncode_core::background::{BackgroundTask, TaskState};
use syncode_core::permission::{ActionClass, ActionRequest};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use syncode_sandbox::{Policy, ProcessContainer};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 每条流 (stdout / stderr 各自) 的字节上限。
const MAX_OUTPUT: usize = 30_000;
/// 进程数默认上限 (Windows Job): 远高于任何真实并行构建 (cargo -j), 但挡得住 fork bomb。
const DEFAULT_MAX_PROCESSES: u32 = 1024;
/// 后台任务累积输出上限 (字节): 防长跑的 dev-server 把内存吃爆。
const MAX_BG_OUTPUT: usize = 200_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    /// 按命令内容分类 (中等语义档): build/test/本地 git/读 → 放行, 外发/不可逆/未识别 → Ask。
    /// 总是返回 `Some` (每条命令都过闸); 安全多数被分到 Allow 档, 故全自动跑得动。
    fn classify(&self, args: &Value) -> Option<ActionRequest> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        Some(classify_command(command))
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its stdout, stderr, and exit code.\n\
         Usage:\n\
         - Runs from the current working directory in a fresh shell: PowerShell on Windows (pwsh if \
         installed, else Windows PowerShell), sh -c on Unix.\n\
         - On Windows write PowerShell, NOT POSIX: use Get-ChildItem / Select-Object / -Recurse / \
         Where-Object, not `ls -la`, `head`, `grep`, or `cmd /c` (those POSIX tools are unavailable \
         unless you pass shell:'bash'). Chain steps with `;`; `&&`/`||` need pwsh (PowerShell 7).\n\
         - The working directory and shell state do NOT persist between calls (each call is a fresh \
         shell); use absolute paths, or chain steps in one command (`;`, and `&&` on Unix/pwsh).\n\
         - stdout and stderr are returned as separate blocks (not chronologically interleaved); each \
         is truncated to ~30000 bytes.\n\
         - Optional timeout_ms (default 120000, max 600000); on timeout the command is killed and \
         the output produced so far is returned. On Windows the whole process tree is killed; on \
         Unix only the direct child is (for now).\n\
         - Prefer the dedicated tools over shell equivalents: Read/Write/Edit for files, Glob to list \
         files or explore the tree, Grep/AstGrep for search, Lsp for code intelligence. Use Bash for \
         builds, tests, git, package managers, and other commands. Quote paths that contain spaces.\n\
         - Optional `shell` selects the interpreter: 'auto' (default; cmd.exe on Windows, sh on Unix), \
         'cmd', 'powershell'/'pwsh' (Windows PowerShell — different quoting and cmdlets), 'sh', 'bash'.\n\
         - Optional `max_memory_mb` caps the command's memory (Windows job memory / Unix RLIMIT_AS); \
         `max_processes` caps live processes (Windows; default 1024, build-safe, stops fork bombs).\n\
         - Set run_in_background:true for a long-running command (dev server, watch, long build): it \
         returns immediately with a task id; use the BashOutput tool with that id to read new output, \
         check status, or kill it. The timeout does not apply to background tasks.\n\
         - A non-zero exit code is reported in the output, not raised as a tool error."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default 120000, max 600000)." },
                "shell": { "type": "string", "enum": ["auto", "cmd", "powershell", "pwsh", "sh", "bash"], "description": "Interpreter to run the command in. Default 'auto' = PowerShell on Windows (pwsh if installed, else Windows PowerShell), sh on Unix. Pass 'bash' for POSIX syntax (ls/head/grep) if bash is installed, or 'cmd' for cmd.exe." },
                "max_memory_mb": { "type": "integer", "description": "Optional memory cap in MiB (Windows job memory / Unix RLIMIT_AS). Default: unlimited." },
                "max_processes": { "type": "integer", "description": "Optional cap on live processes (Windows job). Default 1024." },
                "run_in_background": { "type": "boolean", "description": "Run detached and return a task id immediately; poll/kill it with the BashOutput tool. For long-running commands." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("command is required".into()))?;
        // 0 / 非法 → 默认; 然后夹到 [MIN, MAX] (避免 0 在 spawn 后瞬杀, 也防超上限)。
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .filter(|&t| t > 0)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);

        let shell = args.get("shell").and_then(Value::as_str).unwrap_or("auto");
        let run_in_background =
            args.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
        // 资源硬限 (§7.4): 进程数默认 1024 (build-safe, 挡 fork bomb); 内存 opt-in (默认无限, 免误伤大 linker)。
        // max_processes=0 → 不限 (None), 而非 cap 到 0 (否则一个进程都起不了, review fix)。
        let max_processes = match args.get("max_processes").and_then(Value::as_u64) {
            Some(0) => None,
            Some(v) => Some(v as u32),
            None => Some(DEFAULT_MAX_PROCESSES),
        };
        let max_memory_bytes = args
            .get("max_memory_mb")
            .and_then(Value::as_u64)
            .map(|mb| mb.saturating_mul(1024 * 1024));

        let mut cmd = shell_command(command, shell)?;
        cmd.current_dir(&ctx.cwd);
        // 默认收紧 env (§7.1): 清空后只放行必需项, 杜绝 DEEPSEEK_API_KEY 等机密泄给模型给的命令。
        scrub_env(&mut cmd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Unix: 让子进程自成进程组组长, 供超时时 killpg 杀整组 (Windows 走 Job Object, 无此调用)。
        #[cfg(unix)]
        cmd.process_group(0);
        // Unix 内存硬限: 在子进程 (pre-exec) 里 setrlimit(RLIMIT_AS) —— 仅 max_memory 给定时生效。
        // ⚠️ eyeball-only (tools crate 因 tree-sitter C 依赖无法 cross-check), 运行时待真机验。
        #[cfg(unix)]
        {
            let mem = max_memory_bytes;
            // SAFETY: apply_rlimits 只调 async-signal-safe 的 setrlimit, 在 pre_exec 钩子里安全。
            unsafe {
                cmd.pre_exec(move || syncode_sandbox::apply_rlimits(mem));
            }
        }

        // 进程容器: Windows Job Object 施加 max_processes/max_memory 硬限 + 杀整树; Unix 进程组杀整树。
        let policy = Policy { max_processes, max_memory_bytes, ..Policy::default() };
        let container = ProcessContainer::new(&policy)
            .map_err(|e| ToolError::Exec(format!("sandbox init failed: {e}")))?;

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Exec(format!("failed to spawn command: {e}")))?;

        // spawn 后尽快纳入容器: Windows 传进程 HANDLE, Unix 传 pid (= pgid)。
        // contain 失败 (如本进程已在禁止嵌套的父 Job 里) → 容器管不住该进程: 超时 kill 变 no-op、资源限失效。
        // 不冒进——杀掉刚 spawn 的子进程并报错, 而非静默 `let _ =` 继续 (review fix)。
        // ⚠️ 已知窄窗 (押后, review #7): 子进程在 spawn 与 assign 之间抢先 fork 的孙进程会落在 Job 外。
        // 彻底封堵需 CREATE_SUSPENDED → assign → ResumeThread, 但 tokio::process 不暴露主线程句柄、做不了
        // ResumeThread, 要绕 tokio 直接 CreateProcessW 重写 spawn。窗口仅几条指令, 留待该重写。
        #[cfg(windows)]
        if let Some(handle) = child.raw_handle() {
            if let Err(e) = container.contain(handle as isize) {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                return Err(ToolError::Exec(format!(
                    "could not place the command in a process container ({e}); refusing to run it \
                     ungoverned (timeout-kill and resource limits would not apply)"
                )));
            }
        }
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            // Unix contain() 只存 pgid、不会失败; 仍按 Result 处理以防后端改动。
            if let Err(e) = container.contain(pid as isize) {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                return Err(ToolError::Exec(format!("could not contain the process group ({e})")));
            }
        }

        // 后台模式 (§5.5): 不在此 turn 等结束 —— 起一个抽水任务把输出增量灌进注册表, 立刻返回 task id。
        // 容器用 Arc 共享: drain 任务持一份保活 (关 Job handle 会杀整树!), kill 闭包持一份供 BashOutput 杀。
        if run_in_background {
            let container = Arc::new(container);
            let kill_container = container.clone();
            let task = BackgroundTask::new(command, Box::new(move || kill_container.kill()));
            let id = ctx.background.register(task.clone());

            let mut child = child;
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let drain = task.clone();
            tokio::spawn(async move {
                let _keep = container; // 运行期保活 Job handle
                let pump_out = async {
                    if let Some(r) = stdout {
                        pump(r, &drain).await;
                    }
                };
                let pump_err = async {
                    if let Some(r) = stderr {
                        pump(r, &drain).await;
                    }
                };
                let waiter = async {
                    let state = match child.wait().await {
                        Ok(s) => TaskState::Exited(s.code()),
                        Err(_) => TaskState::Exited(None),
                    };
                    // 终态一次性闩: 已被 BashOutput 杀过 (Killed) 则不被 Exited 盖掉 (review fix #9)。
                    drain.set_terminal(state);
                };
                tokio::join!(pump_out, pump_err, waiter);
            });

            return Ok(ToolOutput::ok(format!(
                "Started in background as `{id}`. Use the BashOutput tool with id \"{id}\" to read \
                 new output incrementally, check its status, or kill it."
            )));
        }

        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();

        // 并发抽干两条管道 + 等退出; 整体套超时。超时则容器杀整树, out/err 保留已抽到的部分。
        let run = async {
            let a = stdout.read_to_end(&mut out);
            let b = stderr.read_to_end(&mut err);
            let w = child.wait();
            tokio::join!(a, b, w)
        };
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), run).await;
        // result 拿到后, run future 已 drop, 对 child/out/err 的借用释放。

        let (exit_code, timed_out) = match result {
            Ok((_, _, wait_res)) => (wait_res.ok().and_then(|s| s.code()), false),
            Err(_) => {
                container.kill(); // 杀整树
                let _ = child.start_kill(); // 兜底直接子进程
                // 收尸, 但**绝不无限等** (即便 contain 失败 / 进程赖着不死, 也不能挂死整个 turn)。
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                (None, true)
            }
        };

        Ok(ToolOutput::ok(format_result(
            &out, &err, exit_code, timed_out, timeout_ms,
        )))
    }
}

/// 选 shell 并构造命令。`shell` = auto/cmd/powershell/pwsh/sh/bash; auto = Windows `cmd /C` / 其他 `sh -c`。
/// PowerShell 走 `-NoProfile -NonInteractive -Command` (不加载用户 profile、不交互、防挂)。
fn shell_command(command: &str, shell: &str) -> Result<Command, ToolError> {
    let kind = if shell == "auto" { default_shell() } else { shell };
    let cmd = match kind {
        "cmd" => {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        }
        "powershell" => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
            c
        }
        "pwsh" => {
            let mut c = Command::new("pwsh");
            c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
            c
        }
        "sh" => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        }
        "bash" => {
            let mut c = Command::new("bash");
            c.arg("-c").arg(command);
            c
        }
        other => {
            return Err(ToolError::InvalidArgs(format!(
                "unknown shell '{other}'; use one of: auto, cmd, powershell, pwsh, sh, bash"
            )));
        }
    };
    Ok(cmd)
}

/// `shell:"auto"` 的平台默认: Unix → `sh`; Windows → PowerShell。
/// **不再默认 cmd.exe** —— 模型本能写 PowerShell/POSIX, cmd 二者都不沾, 一跑就「not recognized」。
/// 优先 `pwsh` (PowerShell 7, 支持 `&&`/`||`), 没装则退回内置 `powershell` (5.1, 总是在)。
#[cfg(windows)]
fn default_shell() -> &'static str {
    if pwsh_on_path() {
        "pwsh"
    } else {
        "powershell"
    }
}

#[cfg(not(windows))]
fn default_shell() -> &'static str {
    "sh"
}

/// PATH 上是否有 `pwsh.exe` (PowerShell 7)。纯查找、不 spawn。
#[cfg(windows)]
fn pwsh_on_path() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join("pwsh.exe").exists()))
        .unwrap_or(false)
}

/// 把一个异步读流**增量**抽进后台任务的输出缓冲 (到 `MAX_BG_OUTPUT` 上限即停止追加)。
async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, task: &BackgroundTask) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => task.append(&String::from_utf8_lossy(&buf[..n]), MAX_BG_OUTPUT),
        }
    }
}

/// 默认收紧子进程 env (§7.1 默认收紧、显式放开): 清空后只回填「shell 能起来 + 跑得了构建」的必需项,
/// **绝不**把 `DEEPSEEK_API_KEY` 等机密暴露给模型给的命令 (一条 `printenv` 就能外泄)。
fn scrub_env(cmd: &mut Command) {
    use std::env;
    cmd.env_clear();
    if let Ok(p) = env::var("PATH") {
        cmd.env("PATH", p);
    }
    let essentials: &[&str] = if cfg!(windows) {
        &[
            // cmd.exe 起不来若缺这些。
            "SystemRoot", "windir", "ComSpec", "PATHEXT", "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            // 临时目录 + 用户目录。
            "TEMP", "TMP", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "LOCALAPPDATA", "APPDATA",
            // PowerShell 模块发现 (shell=powershell/pwsh 时需要; 非机密)。
            "PSModulePath",
            // **工具链发现**: rustc/cc 靠 vswhere 找 MSVC linker, vswhere 在 %ProgramFiles(x86)%;
            // 缺这些则 `link.exe not found` —— 不是机密, 必须放行 (实证教训)。
            "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData",
        ]
    } else {
        &["HOME", "TMPDIR", "LANG", "LC_ALL", "USER"]
    };
    for k in essentials.iter().chain(["CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"].iter()) {
        if let Ok(v) = env::var(k) {
            cmd.env(k, v);
        }
    }
}

/// 拼 stdout + stderr (各自先独立截断, 免大 stdout 把 stderr 挤没) + 退出码/超时标注。
fn format_result(out: &[u8], err: &[u8], exit_code: Option<i32>, timed_out: bool, timeout_ms: u64) -> String {
    let stdout = truncate_stream(&String::from_utf8_lossy(out), "stdout");
    let stderr = truncate_stream(&String::from_utf8_lossy(err), "stderr");

    let mut s = String::new();
    if !stdout.trim().is_empty() {
        s.push_str(&stdout);
    }
    if !stderr.trim().is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&stderr);
    }

    let footer = if timed_out {
        format!("[command timed out after {timeout_ms}ms and was killed]")
    } else {
        match exit_code {
            Some(0) => String::new(),
            Some(code) => format!("[exit code {code}]"),
            None => "[process did not return an exit code (killed or signaled)]".to_string(),
        }
    };
    if !footer.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&footer);
    }

    if s.trim().is_empty() {
        "(command produced no output; exit code 0)".to_string()
    } else {
        s
    }
}

/// 单条流按 `MAX_OUTPUT` 字节截断, 保 char 边界, 附标注。
fn truncate_stream(s: &str, label: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let mut end = MAX_OUTPUT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[{label} truncated at {MAX_OUTPUT} bytes]", &s[..end])
}

// ───────────────────────── 命令分类 (审批 §7.5, 中等语义档) ─────────────────────────
//
// 把一条 shell 命令按「可逆性 / 影响面」定级, 供 [`PolicyApprover`](syncode_core::permission) 判要不要叫人。
// **非安全边界**: shell 能 `;`/`$(...)`/别名/引号绕过这套前缀解析, 真约束靠沙箱 (路线图 P1/P2)。
// 故策略偏保守: 拆成多段、取最危险段、未识别 → `ArbitraryExec` (fail-closed Ask)。

/// 把整条命令分类成一个 [`ActionRequest`]: 拆成段, 每段定级, 取**最危险**段。
fn classify_command(command: &str) -> ActionRequest {
    let mut best: Option<(ActionClass, Option<String>)> = None;
    for seg in split_segments(command) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let cand = classify_segment(seg);
        let take = match &best {
            None => true,
            Some((w, _)) => severity(&cand.0) > severity(w),
        };
        if take {
            best = Some(cand);
        }
    }
    let (class, path) = best.unwrap_or((ActionClass::ArbitraryExec, None));
    // 写 / 本地执行: target = 具体路径 (None = 项目内 → 放行); 其它类: target = 命令本身 (供展示)。
    let target = match class {
        ActionClass::WriteFs | ActionClass::LocalExec => path,
        _ => Some(truncate_cmd(command)),
    };
    ActionRequest { class, tool: "Bash".to_string(), target }
}

/// 危险度排序: Ask 档 (≥5) 一律高于 Allow 档, 故组合时「最危险段」胜出。
fn severity(c: &ActionClass) -> u8 {
    use ActionClass::*;
    match c {
        ReadFs => 0,
        Build | RunTests | VcsLocal => 1,
        WriteFs => 2,
        LocalExec => 3,
        ArbitraryExec | Other(_) => 5,
        InstallDeps | Network | VcsPublish => 6,
        Destructive => 7,
        Privileged => 8,
    }
}

/// 朴素拆段: 在 `&&` `||` `;` `|` `&` 换行 处切 (不处理引号内的操作符 —— 偏多拆 = 偏保守)。
/// 但**不**把属于重定向的 `&`/`|` 当分隔符 (`2>&1`, `>&2`, `&>file`, `>|file`), 否则 `cargo build 2>&1`
/// 会被 `&` 拆碎、误判成 ArbitraryExec (review fix)。
fn split_segments(cmd: &str) -> Vec<&str> {
    let bytes = cmd.as_bytes();
    let mut segs = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let two = bytes.get(i).zip(bytes.get(i + 1));
        let is_two_op = matches!(two, Some((b'&', b'&')) | Some((b'|', b'|')));
        if is_two_op {
            segs.push(&cmd[start..i]);
            i += 2;
            start = i;
            continue;
        }
        let prev = if i > 0 { Some(bytes[i - 1]) } else { None };
        let next = bytes.get(i + 1).copied();
        let is_one_op = match bytes[i] {
            b';' | b'\n' => true,
            // `&` 是后台/分隔符, 但 `>&`/`&>`/`2>&1` 里的 `&` 属于重定向, 不拆。
            b'&' => !(prev == Some(b'>') || next == Some(b'>')),
            // `|` 是管道, 但 `>|` (clobber 重定向) 里的 `|` 不拆。
            b'|' => prev != Some(b'>'),
            _ => false,
        };
        if is_one_op {
            segs.push(&cmd[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < cmd.len() {
        segs.push(&cmd[start..]);
    }
    segs
}

/// 给一段 (无 shell 操作符) 命令定级, 返回 (类别, 影响面路径)。
/// 除按程序名分类外, 还覆盖两类前缀解析的盲点 (取最危险者):
/// - **输出重定向** (`>` `>>` `2>` `&>`): 目标其实是被写文件, 由生产命令 (echo/cat/curl…) 分类会漏 →
///   按写处理 (绝对/~/.. 目标带路径交审批做根判, 相对项目内放行)。
/// - **命令替换** (`$(...)` / 反引号): 内嵌任意命令, 前缀解析看不到 → 保守升 ArbitraryExec (fail-closed Ask)。
fn classify_segment(seg: &str) -> (ActionClass, Option<String>) {
    let tokens: Vec<&str> = seg.split_whitespace().collect();
    // 跳过前导环境赋值 (`FOO=bar cmd ...`), 取第一个真正的程序名。
    let Some(pi) = tokens.iter().position(|t| !is_env_assign(t)) else {
        return (ActionClass::ReadFs, None);
    };
    let raw = strip_quotes(tokens[pi]);
    let args = &tokens[pi + 1..];

    let mut cand = classify_program(raw, args);

    // 命令替换: 内嵌任意命令 → 升 ArbitraryExec (Ask)。(`${VAR}` 是参数展开, 非执行, 不算。)
    if seg.contains("$(") || seg.contains('`') {
        cand = more_severe(cand, (ActionClass::ArbitraryExec, None));
    }
    // 输出重定向: 目标文件被写。绝对/~/.. 目标带上交审批做写根判; 相对项目内 → WriteFs(None) 放行。
    if let Some(rt) = find_redirect_target(&tokens) {
        let target = if is_escaping_path(&rt) { Some(rt) } else { None };
        cand = more_severe(cand, (ActionClass::WriteFs, target));
    }
    cand
}

/// 取严重度更高者 (相等取 a)。
fn more_severe(
    a: (ActionClass, Option<String>),
    b: (ActionClass, Option<String>),
) -> (ActionClass, Option<String>) {
    if severity(&b.0) > severity(&a.0) {
        b
    } else {
        a
    }
}

/// 输出重定向目标 (被写的文件路径)。识别 `> file` / `>file` / `>>out` / `2>err` / `&>all`;
/// 跳过 fd 复制 (`2>&1`)。
fn find_redirect_target(tokens: &[&str]) -> Option<String> {
    for (i, &t) in tokens.iter().enumerate() {
        if matches!(t, ">" | ">>" | "1>" | "2>" | "&>" | "1>>" | "2>>" | ">|") {
            if let Some(&next) = tokens.get(i + 1) {
                let tgt = strip_quotes(next);
                if is_real_write_target(tgt) {
                    return Some(tgt.to_string());
                }
            }
            continue;
        }
        for op in [">>", "2>>", "1>>", "2>", "1>", "&>", ">"] {
            if let Some(rest) = t.strip_prefix(op) {
                let tgt = strip_quotes(rest);
                if is_real_write_target(tgt) {
                    return Some(tgt.to_string());
                }
            }
        }
    }
    None
}

/// 重定向目标是否是「真写一个文件」(而非 fd 复制 / null 设备汇)。
fn is_real_write_target(t: &str) -> bool {
    !t.is_empty() && !is_fd_dup(t) && !is_null_sink(t)
}

/// fd 复制 / 非文件目标 (`&1`, 纯数字 fd) —— 不算文件写。
fn is_fd_dup(t: &str) -> bool {
    t.starts_with('&') || (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
}

/// null / 设备汇 (`/dev/null`、`2>/dev/null`、Windows `NUL` …) —— 极常见且无害, 不当文件写 (实跑发现)。
fn is_null_sink(t: &str) -> bool {
    let l = t.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "/dev/null" | "/dev/zero" | "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "nul" | "nul:"
    )
}

/// 按程序名给一段定级 (不含重定向 / 替换处理)。
fn classify_program(raw: &str, args: &[&str]) -> (ActionClass, Option<String>) {
    // 直接执行二进制 / 编译产物: ./x, .\x, ../x, 绝对路径, 或裸 *.exe/.out/.bin... → LocalExec。
    if let Some(target) = local_exec_target(raw) {
        return (ActionClass::LocalExec, target);
    }

    let prog = prog_name(raw);
    match prog.as_str() {
        "sudo" | "su" | "doas" | "runas" => (ActionClass::Privileged, None),
        "shutdown" | "reboot" | "halt" | "poweroff" | "init" | "kill" | "killall" | "pkill"
        | "taskkill" | "stop-computer" | "restart-computer" | "stop-process" => {
            (ActionClass::Privileged, None)
        }
        "git" => classify_git(args),
        "cargo" => (classify_cargo(args), None),
        "go" => (classify_go(args), None),
        "npm" | "pnpm" | "yarn" | "bun" => (classify_node(args), None),
        "pip" | "pip3" | "pipx" | "conda" => (classify_pip(args), None),
        "apt" | "apt-get" | "dnf" | "yum" | "zypper" | "pacman" | "apk" | "brew" | "choco"
        | "scoop" | "winget" | "gem" => (ActionClass::InstallDeps, None),
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "ftp" | "nc" | "ncat" | "netcat" | "telnet"
        | "rsync" | "invoke-webrequest" | "iwr" | "invoke-restmethod" | "irm" => {
            (ActionClass::Network, None)
        }
        "rm" | "del" | "erase" | "rmdir" | "rd" | "remove-item" | "ri" => classify_rm(args),
        // `tee` 是写工具 (写它的文件参数), 不是读 → 走 fsmutate 做根判 (review fix)。
        "mkdir" | "md" | "touch" | "cp" | "copy" | "mv" | "move" | "ren" | "rename" | "ln"
        | "chmod" | "chown" | "attrib" | "icacls" | "new-item" | "ni" | "set-content"
        | "out-file" | "copy-item" | "move-item" | "add-content" | "tee" => classify_fsmutate(args),
        "rustc" | "cc" | "gcc" | "g++" | "clang" | "clang++" | "make" | "cmake" | "ninja"
        | "tsc" | "mvn" | "gradle" | "javac" | "dotnet" | "msbuild" => (ActionClass::Build, None),
        "pytest" | "jest" | "mocha" | "vitest" | "nextest" | "phpunit" | "rspec" | "tox"
        | "ctest" => (ActionClass::RunTests, None),
        // 注意: `env` / `xargs` 是 runner (执行被包裹的程序), 已移出 ReadFs → 落到 ArbitraryExec
        // (fail-closed Ask), 免被包裹的 Network/exec 被当读放行 (review fix)。
        "ls" | "dir" | "pwd" | "cd" | "echo" | "cat" | "type" | "head" | "tail" | "wc" | "grep"
        | "egrep" | "fgrep" | "findstr" | "rg" | "find" | "which" | "where" | "whoami"
        | "hostname" | "date" | "printenv" | "set" | "tree" | "stat" | "file" | "du"
        | "df" | "uname" | "ver" | "sleep" | "true" | "false" | "clear" | "cls" | "basename"
        | "dirname" | "realpath" | "readlink" | "sort" | "uniq" | "cut" | "diff" | "cmp"
        | "tr"
        // PowerShell 读类 / 管道转换类 cmdlet 与别名 (纯读 / 纯数据变换, 不写不执行)。
        // 补全常见管道 cmdlet (Select-Object/Sort-Object/Format-*/…), 否则 `gci | select …`
        // 这类只读管道里任一段不认识 → ArbitraryExec → 每次弹审批 (用户实测痛点)。
        | "get-childitem" | "gci" | "get-content" | "gc" | "select-string" | "sls"
        | "test-path" | "get-item" | "gi" | "get-location" | "gl" | "measure-object" | "measure"
        | "write-output" | "write-host" | "get-process" | "where-object" | "foreach-object"
        | "select-object" | "select" | "sort-object" | "format-table" | "ft" | "format-list"
        | "fl" | "format-wide" | "fw" | "out-string" | "out-host" | "group-object" | "group"
        | "get-member" | "gm" | "get-command" | "gcm" | "resolve-path" | "rvpa" | "split-path"
        | "join-path" | "get-itemproperty" | "gp" | "convertto-json" | "convertfrom-json"
        | "convertto-csv" | "convertfrom-csv" | "compare-object" | "compare" | "get-date"
        | "get-alias" | "get-variable" | "gv" | "get-help" | "get-module" | "get-unique" | "gu"
        // ForEach-Object / Where-Object 的符号别名 (% / ?) —— 与上面同类 (既有决策)。
        | "%" | "?" => {
            (ActionClass::ReadFs, None)
        }
        _ => (ActionClass::ArbitraryExec, None),
    }
}

/// 是否为直接执行的路径 / 本地产物。`Some(None)` = 项目内 (放行); `Some(Some(p))` = 可能出根 (交审批判)。
fn local_exec_target(raw: &str) -> Option<Option<String>> {
    if raw.starts_with("./") || raw.starts_with(".\\") {
        return Some(None);
    }
    if raw.starts_with("../") || raw.starts_with("..\\") {
        return Some(Some(raw.to_string()));
    }
    if is_absolute_path(raw) {
        return Some(Some(raw.to_string()));
    }
    // 裸的编译产物 (sum.exe / app.out / x.bin), 非 PATH 工具且不含路径分隔 → 当作项目内本地执行。
    // 脚本扩展 (.sh/.py/.bat/.ps1) 不入此列: 跑脚本 = 任意代码, 交由 ArbitraryExec → Ask。
    if has_artifact_ext(raw) && !raw.contains('/') && !raw.contains('\\') {
        return Some(None);
    }
    None
}

fn has_artifact_ext(raw: &str) -> bool {
    let l = raw.to_ascii_lowercase();
    [".exe", ".out", ".bin", ".run", ".com", ".app"].iter().any(|e| l.ends_with(e))
}

fn is_absolute_path(r: &str) -> bool {
    r.starts_with('/')
        || r.starts_with('~')
        || r.starts_with("\\\\")
        || {
            let b = r.as_bytes();
            b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
        }
}

/// 程序名: basename + 去掉 windows 可执行后缀 + 小写。
fn prog_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let lower = base.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com", ".ps1"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    lower
}

fn strip_quotes(t: &str) -> &str {
    t.trim_matches(['"', '\''])
}

fn is_env_assign(t: &str) -> bool {
    if t.starts_with('-') {
        return false;
    }
    match t.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.contains('/')
                && !k.contains('\\')
        }
        None => false,
    }
}

/// 第 `n` 个非 flag 参数 (跳过 `-x` / `--x`)。
fn nth_non_flag<'a>(args: &[&'a str], n: usize) -> Option<&'a str> {
    args.iter().filter(|a| !a.starts_with('-')).nth(n).map(|&a| strip_quotes(a))
}

/// 参数里第一个「逃出当前目录」的路径 (绝对 / `~` / `..`); flag 跳过。
fn first_escaping_path(args: &[&str]) -> Option<String> {
    args.iter()
        .map(|&a| strip_quotes(a))
        .find(|a| is_escaping_path(a))
        .map(|s| s.to_string())
}

fn is_escaping_path(a: &str) -> bool {
    if a.starts_with('-') {
        return false;
    }
    is_absolute_path(a)
        || a == ".."
        || a.starts_with("../")
        || a.starts_with("..\\")
        || a.contains("/../")
        || a.contains("\\..\\")
}

fn classify_git(args: &[&str]) -> (ActionClass, Option<String>) {
    match nth_non_flag(args, 0) {
        Some("push") => (ActionClass::VcsPublish, None),
        Some("remote") => {
            let mutating = args.iter().any(|&a| {
                matches!(
                    strip_quotes(a),
                    "set-url" | "add" | "remove" | "rm" | "rename" | "set-branches" | "set-head"
                )
            });
            (if mutating { ActionClass::VcsPublish } else { ActionClass::VcsLocal }, None)
        }
        Some("clone") | Some("fetch") | Some("pull") => (ActionClass::Network, None),
        // status/diff/log/add/commit/checkout/branch/stash/reset/... 及未知 git 子命令 → 仓库内。
        _ => (ActionClass::VcsLocal, None),
    }
}

fn classify_cargo(args: &[&str]) -> ActionClass {
    match nth_non_flag(args, 0) {
        Some("install") | Some("uninstall") => ActionClass::InstallDeps,
        Some("publish") | Some("login") | Some("owner") | Some("yank") => ActionClass::VcsPublish,
        // add/remove/update 改依赖 + 联网拉 = 供应链面。
        Some("add") | Some("remove") | Some("rm") | Some("update") => ActionClass::InstallDeps,
        Some("test") | Some("t") | Some("bench") | Some("nextest") => ActionClass::RunTests,
        // build/check/clippy/fmt/doc/run/clean/tree/... 及未知子命令 → 可逆、项目内。
        _ => ActionClass::Build,
    }
}

fn classify_go(args: &[&str]) -> ActionClass {
    match nth_non_flag(args, 0) {
        Some("install") | Some("get") => ActionClass::InstallDeps,
        Some("test") => ActionClass::RunTests,
        _ => ActionClass::Build,
    }
}

fn classify_node(args: &[&str]) -> ActionClass {
    match nth_non_flag(args, 0) {
        Some("install") | Some("i") | Some("add") | Some("ci") | Some("update") | Some("up")
        | Some("upgrade") | Some("dedupe") => ActionClass::InstallDeps,
        Some("publish") => ActionClass::VcsPublish,
        Some("test") | Some("t") => ActionClass::RunTests,
        Some("ls") | Some("list") | Some("view") | Some("outdated") | Some("audit")
        | Some("ping") | Some("help") => ActionClass::ReadFs,
        Some("run") | Some("run-script") => match nth_non_flag(args, 1) {
            Some("build") | Some("compile") => ActionClass::Build,
            Some("test") => ActionClass::RunTests,
            // npm run <任意脚本> = 跑 package 脚本 = 任意代码。
            _ => ActionClass::ArbitraryExec,
        },
        // exec / npx / dlx / 未知 → 任意执行。
        _ => ActionClass::ArbitraryExec,
    }
}

fn classify_pip(args: &[&str]) -> ActionClass {
    match nth_non_flag(args, 0) {
        Some("install") | Some("uninstall") | Some("download") => ActionClass::InstallDeps,
        // list/show/freeze/check/... → 读。
        _ => ActionClass::ReadFs,
    }
}

fn classify_rm(args: &[&str]) -> (ActionClass, Option<String>) {
    let recursive = args.iter().any(|&a| {
        let l = a.to_ascii_lowercase();
        (l.starts_with('-') && !l.starts_with("--") && l.contains('r'))
            || l == "--recursive"
            || l == "-recurse"
            || l == "/s"
    });
    if recursive || first_escaping_path(args).is_some() {
        // 递归删 / 删项目外 → 不可逆、出沙箱。
        (ActionClass::Destructive, None)
    } else {
        // 项目内单文件删除 → 可从 VCS 恢复, 视为可逆 → 放行。
        (ActionClass::WriteFs, None)
    }
}

fn classify_fsmutate(args: &[&str]) -> (ActionClass, Option<String>) {
    match first_escaping_path(args) {
        Some(p) => (ActionClass::Destructive, Some(p)), // 改项目外路径 → 出沙箱。
        None => (ActionClass::WriteFs, None),           // 项目内 → 放行。
    }
}

fn truncate_cmd(cmd: &str) -> String {
    let c = cmd.trim();
    if c.chars().count() <= 120 {
        c.to_string()
    } else {
        let s: String = c.chars().take(117).collect();
        format!("{s}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncode_core::permission::{Approver, Decision, PolicyApprover};

    fn class_of(cmd: &str) -> ActionClass {
        classify_command(cmd).class
    }

    #[test]
    fn safe_dev_commands_are_allow_classes() {
        assert_eq!(class_of("cargo build"), ActionClass::Build);
        assert_eq!(class_of("cargo build --release"), ActionClass::Build);
        assert_eq!(class_of("cargo run"), ActionClass::Build);
        assert_eq!(class_of("cargo test"), ActionClass::RunTests);
        assert_eq!(class_of("cargo nextest run"), ActionClass::RunTests);
        assert_eq!(class_of("pytest -q"), ActionClass::RunTests);
        assert_eq!(class_of("git status"), ActionClass::VcsLocal);
        assert_eq!(class_of("git commit -m 'x'"), ActionClass::VcsLocal);
        assert_eq!(class_of("git add ."), ActionClass::VcsLocal);
        assert_eq!(class_of("ls -la"), ActionClass::ReadFs);
        assert_eq!(class_of("cat Cargo.toml"), ActionClass::ReadFs);
        assert_eq!(class_of("rustc main.rs -o main"), ActionClass::Build);
        assert_eq!(class_of("mkdir build"), ActionClass::WriteFs);
        assert_eq!(class_of("rm stale.txt"), ActionClass::WriteFs);
    }

    #[test]
    fn outward_and_irreversible_commands_are_ask_classes() {
        assert_eq!(class_of("git push"), ActionClass::VcsPublish);
        assert_eq!(class_of("git push origin main"), ActionClass::VcsPublish);
        assert_eq!(class_of("git remote set-url origin x"), ActionClass::VcsPublish);
        assert_eq!(class_of("cargo publish"), ActionClass::VcsPublish);
        assert_eq!(class_of("npm install left-pad"), ActionClass::InstallDeps);
        assert_eq!(class_of("cargo add serde"), ActionClass::InstallDeps);
        assert_eq!(class_of("pip install requests"), ActionClass::InstallDeps);
        assert_eq!(class_of("apt-get install gcc"), ActionClass::InstallDeps);
        assert_eq!(class_of("curl https://evil.sh"), ActionClass::Network);
        assert_eq!(class_of("git clone https://x"), ActionClass::Network);
        assert_eq!(class_of("rm -rf build"), ActionClass::Destructive);
        assert_eq!(class_of("rm -rf /"), ActionClass::Destructive);
        assert_eq!(class_of("rm /etc/passwd"), ActionClass::Destructive);
        assert_eq!(class_of("cp secret.txt /etc/x"), ActionClass::Destructive);
        assert_eq!(class_of("sudo rm -rf /"), ActionClass::Privileged);
        assert_eq!(class_of("python script.py"), ActionClass::ArbitraryExec);
    }

    #[test]
    fn worst_segment_wins() {
        // 安全段 + 危险段 → 取危险。
        assert_eq!(class_of("echo hi && cargo test"), ActionClass::RunTests);
        assert_eq!(class_of("cargo build && git push"), ActionClass::VcsPublish);
        assert_eq!(class_of("cd /tmp ; rm -rf x"), ActionClass::Destructive);
    }

    #[test]
    fn leading_env_assignment_is_skipped() {
        assert_eq!(class_of("RUST_LOG=debug cargo test"), ActionClass::RunTests);
        assert_eq!(class_of("FOO=bar BAZ=qux git push"), ActionClass::VcsPublish);
    }

    #[test]
    fn local_exec_detection() {
        assert_eq!(class_of("./target/debug/app"), ActionClass::LocalExec);
        assert_eq!(class_of("sum.exe"), ActionClass::LocalExec);
        assert_eq!(class_of("rustc s.rs -o sum.exe && sum.exe"), ActionClass::LocalExec);
        // 绝对路径执行: 是 LocalExec, target 带路径供审批判根内/外。
        let req = classify_command("/usr/bin/curl https://x");
        assert_eq!(req.class, ActionClass::LocalExec);
        assert_eq!(req.target.as_deref(), Some("/usr/bin/curl"));
    }

    #[test]
    fn policy_lets_build_run_in_temp_but_blocks_push() {
        // 模拟 demo: 在临时目录写+编译+跑产物 → 放行; push → 拒。
        let approver = PolicyApprover::new("/proj");
        let tmp = std::env::temp_dir();
        let exe = tmp.join("sum.exe");
        let run = classify_command(&format!("rustc s.rs -o {0} && {0}", exe.to_string_lossy()));
        assert_eq!(approver.decide(&run), Decision::Allow, "running built artifact in temp");
        assert_eq!(approver.decide(&classify_command("cargo test")), Decision::Allow);
        assert_eq!(approver.decide(&classify_command("git push")), Decision::Ask);
        assert_eq!(approver.decide(&classify_command("curl https://x")), Decision::Ask);
    }

    #[test]
    fn powershell_cmdlets_classify() {
        assert_eq!(class_of("Get-ChildItem -Recurse"), ActionClass::ReadFs);
        assert_eq!(class_of("Get-Content README.md"), ActionClass::ReadFs);
        assert_eq!(class_of("Remove-Item -Recurse -Force build"), ActionClass::Destructive);
        assert_eq!(class_of("Invoke-WebRequest https://x -OutFile y"), ActionClass::Network);
        assert_eq!(class_of("New-Item -ItemType File foo.txt"), ActionClass::WriteFs);
        assert_eq!(class_of("Stop-Computer"), ActionClass::Privileged);
    }

    #[test]
    fn powershell_readonly_pipelines_are_allowed_not_asked() {
        // 用户痛点: 只读 PowerShell 管道里有 Select-Object/Sort-Object 等就被判 Ask。修后应全 Allow。
        let approver = PolicyApprover::new("/proj");
        for cmd in [
            "Get-ChildItem -Recurse | Select-Object FullName",
            "Get-ChildItem -Recurse -File | Sort-Object Length | Select-Object -First 50",
            "Get-ChildItem | Where-Object { $_.Name -like '*.rs' } | Measure-Object",
            "Get-Content Cargo.toml | Select-String version",
            "gci -Recurse | % { $_.FullName }",
            "Get-ChildItem | Format-Table Name, Length",
        ] {
            assert_eq!(class_of(cmd), ActionClass::ReadFs, "{cmd}");
            assert_eq!(approver.decide(&classify_command(cmd)), Decision::Allow, "{cmd}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_shell_is_powershell_not_cmd() {
        // 默认不再是 cmd.exe (模型写 PowerShell/POSIX, cmd 二者都跑不了)。
        assert!(matches!(default_shell(), "pwsh" | "powershell"));
    }

    #[test]
    fn shell_selection_and_unknown_shell() {
        // 已知 shell 构造成功 (auto → 平台默认)。
        assert!(shell_command("echo hi", "auto").is_ok());
        assert!(shell_command("echo hi", "pwsh").is_ok());
        assert!(shell_command("echo hi", "powershell").is_ok());
        assert!(shell_command("echo hi", "bash").is_ok());
        // 未知 shell → 明确报错 (不静默回退)。
        let err = shell_command("echo hi", "zsh-nope");
        assert!(err.is_err(), "unknown shell must error");
    }

    #[test]
    fn redirection_and_substitution_are_gated() {
        let approver = PolicyApprover::new("/proj");
        // 重定向到项目外 → WriteFs(target 出根) → Ask (不再被 echo 当读放行)。
        let r = classify_command("echo pwned > /etc/passwd");
        assert_eq!(r.class, ActionClass::WriteFs);
        assert_eq!(approver.decide(&r), Decision::Ask);
        // 重定向到相对项目内 → Allow。
        assert_eq!(approver.decide(&classify_command("echo log > build.txt")), Decision::Allow);
        // cat 出根 → Ask。
        assert_eq!(
            approver.decide(&classify_command("cat secret.txt > ../../outside/loot")),
            Decision::Ask
        );
        // fd 复制不算写: cargo build 2>&1 → 仍 Build → Allow。
        assert_eq!(class_of("cargo build 2>&1"), ActionClass::Build);
        // 命令替换 → ArbitraryExec → Ask。
        assert_eq!(class_of("echo $(curl https://evil.sh)"), ActionClass::ArbitraryExec);
        assert_eq!(approver.decide(&classify_command("echo `curl https://x`")), Decision::Ask);
    }

    #[test]
    fn tee_and_runners_no_longer_slip_through() {
        let approver = PolicyApprover::new("/proj");
        // tee 写项目外 → Destructive → Ask。
        assert_eq!(
            approver.decide(&classify_command("echo x | tee /etc/cron.d/evil")),
            Decision::Ask
        );
        // tee 写项目内 → WriteFs → Allow。
        assert_eq!(approver.decide(&classify_command("echo x | tee out.txt")), Decision::Allow);
        // env / xargs runner → ArbitraryExec → Ask (不再被当读放行)。
        assert_eq!(class_of("env curl https://x -o out"), ActionClass::ArbitraryExec);
        assert_eq!(class_of("echo url | xargs curl"), ActionClass::ArbitraryExec);
    }

    #[test]
    fn null_sinks_are_not_treated_as_writes() {
        // 实跑发现: 2>/dev/null / >NUL 极常见且无害, 不该被重定向门当出根写拦 (review fix from live run)。
        let approver = PolicyApprover::new("/proj");
        assert_eq!(class_of("cargo build 2>/dev/null"), ActionClass::Build);
        assert_eq!(approver.decide(&classify_command("cargo build 2>/dev/null")), Decision::Allow);
        assert_eq!(approver.decide(&classify_command("ls -la > /dev/null")), Decision::Allow);
        assert_eq!(approver.decide(&classify_command("dir > NUL")), Decision::Allow);
        assert_eq!(approver.decide(&classify_command("cargo test 1>/dev/null 2>&1")), Decision::Allow);
    }
}
