//! Bash: `timeout` (上限 600s) + `run_in_background` + **默认带沙箱** (§10)。
//! 万能逃生口 (§5/§6.0 机制①)。真杠杆是「给子进程上沙箱」(`syncode-sandbox`), 不是 spawn 机制。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command. Runs under a sandbox by default; supports a timeout \
         and optional background execution."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to execute." },
                "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (max 600000)." },
                "run_in_background": { "type": "boolean", "description": "Run detached and return immediately." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn is_dangerous(&self) -> bool {
        // 任意命令执行 = 最大边界 (§3.1)。必须子进程 + 沙箱 + 审批 (§5.2)。
        true
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        todo!("spawn under sandbox via tokio::process + portable-pty (guide §4/§7)")
    }
}
