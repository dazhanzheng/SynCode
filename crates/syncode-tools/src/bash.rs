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
const MAX_TIMEOUT_MS: u64 = 600_000;
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
        "Execute a shell command and return its combined stdout/stderr plus the exit code.\n\
         Usage:\n\
         - Runs in a shell (cmd on Windows, sh on Unix) from the current working directory.\n\
         - The process runs inside a kill-on-close container, so a timeout reliably terminates the \
         whole process tree (no orphaned child processes).\n\
         - Optional timeout_ms (default 120000, max 600000); on timeout the command is killed and \
         whatever output was produced so far is returned.\n\
         - Prefer the dedicated tools over shell equivalents: Read/Write/Edit for files, Grep/AstGrep \
         for search, Lsp for code intelligence. Use Bash for builds, tests, git, and other commands.\n\
         - Output is truncated to 30000 bytes."
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
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let mut cmd = shell_command(command);
        cmd.current_dir(&ctx.cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // 进程容器 (Windows Job Object: 关 handle 即杀整树)。v1 不设进程/内存硬限 (免误伤 cargo)。
        let container = ProcessContainer::new(&Policy::default())
            .map_err(|e| ToolError::Exec(format!("sandbox init failed: {e}")))?;

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Exec(format!("failed to spawn command: {e}")))?;

        // spawn 后尽快纳入容器 (Windows)。
        #[cfg(windows)]
        if let Some(handle) = child.raw_handle() {
            let _ = container.contain(handle as isize);
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
                let _ = child.wait().await; // 收尸, 免 zombie
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

/// 拼 stdout+stderr + 退出码/超时标注, 截断到 `MAX_OUTPUT`。
fn format_result(out: &[u8], err: &[u8], exit_code: Option<i32>, timed_out: bool, timeout_ms: u64) -> String {
    let stdout = String::from_utf8_lossy(out);
    let stderr = String::from_utf8_lossy(err);

    let mut body = String::new();
    if !stdout.trim().is_empty() {
        body.push_str(&stdout);
    }
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    let mut s = truncate(&body, MAX_OUTPUT);

    if timed_out {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&format!("[command timed out after {timeout_ms}ms and the process tree was killed]"));
    } else {
        match exit_code {
            Some(0) => {}
            Some(code) => {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&format!("[exit code {code}]"));
            }
            None => {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str("[process terminated without an exit code]");
            }
        }
    }

    if s.trim().is_empty() {
        "(command produced no output; exit code 0)".to_string()
    } else {
        s
    }
}

/// 按字节上限截断, 保 char 边界, 附省略提示。
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated at {max} bytes]", &s[..end])
}
