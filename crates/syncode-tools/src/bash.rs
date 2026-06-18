//! Bash: 跑 shell 命令 (架构 §6.0 机制① 万能逃生口, §10)。
//!
//! 让 agent 能跑 build/test/git —— 补上「写→跑→看失败→改」验证闭环里缺的那环。
//! 子进程塞进 [`ProcessContainer`](syncode_sandbox::ProcessContainer): Windows 上是 Job Object,
//! **超时/取消时杀整棵进程树**(`cargo test` 会 spawn `rustc`/test 子进程, 朴素 kill 会留孤儿)。
//! `is_dangerous=true` —— 任意命令执行 = 最大边界 (§3.1), 走审批闸 (§5.2); 沙箱能力面随后加严。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use syncode_sandbox::{Policy, ProcessContainer};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 每条流 (stdout / stderr 各自) 的字节上限。
const MAX_OUTPUT: usize = 30_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn is_dangerous(&self) -> bool {
        // 任意命令执行 = 最大边界 (§3.1)。子进程 + 沙箱容器 + 审批 (§5.2)。
        true
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its stdout, stderr, and exit code.\n\
         Usage:\n\
         - Runs from the current working directory in a shell: cmd.exe (/C) on Windows, sh -c on \
         Unix. On Windows use cmd.exe syntax, not POSIX sh.\n\
         - The working directory and shell state do NOT persist between calls (each call is a fresh \
         shell); use absolute paths, or chain steps in one command with && / ;.\n\
         - stdout and stderr are returned as separate blocks (not chronologically interleaved); each \
         is truncated to ~30000 bytes.\n\
         - Optional timeout_ms (default 120000, max 600000); on timeout the command is killed and \
         the output produced so far is returned. On Windows the whole process tree is killed; on \
         Unix only the direct child is (for now).\n\
         - Prefer the dedicated tools over shell equivalents: Read/Write/Edit for files, Grep/AstGrep \
         for search, Lsp for code intelligence. Use Bash for builds, tests, git, package managers, \
         and other commands. Quote paths that contain spaces.\n\
         - A non-zero exit code is reported in the output, not raised as a tool error."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default 120000, max 600000)." }
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

        let mut cmd = shell_command(command);
        cmd.current_dir(&ctx.cwd);
        // 默认收紧 env (§7.1): 清空后只放行必需项, 杜绝 DEEPSEEK_API_KEY 等机密泄给模型给的命令。
        scrub_env(&mut cmd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Unix: 让子进程自成进程组组长, 供超时时 killpg 杀整组 (Windows 走 Job Object, 无此调用)。
        #[cfg(unix)]
        cmd.process_group(0);

        // 进程容器 (Windows Job Object / Unix 进程组)。v1 不设进程/内存硬限 (免误伤 cargo)。
        let container = ProcessContainer::new(&Policy::default())
            .map_err(|e| ToolError::Exec(format!("sandbox init failed: {e}")))?;

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Exec(format!("failed to spawn command: {e}")))?;

        // spawn 后尽快纳入容器: Windows 传进程 HANDLE, Unix 传 pid (= pgid)。
        #[cfg(windows)]
        if let Some(handle) = child.raw_handle() {
            let _ = container.contain(handle as isize);
        }
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = container.contain(pid as isize);
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

/// 平台 shell: Windows `cmd /C`, 其他 `sh -c`。
fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
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
        // cmd.exe 起不来若缺 SystemRoot/ComSpec/PATHEXT。
        &[
            "SystemRoot", "windir", "TEMP", "TMP", "USERPROFILE", "ComSpec", "PATHEXT",
            "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE",
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
