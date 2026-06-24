//! BashOutput: 读后台 Bash 任务的**增量**输出 / 查状态 / 杀 (配合 Bash 的 `run_in_background`, §5.5)。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct BashOutputTool;

#[async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "BashOutput"
    }

    fn description(&self) -> &str {
        "Read new output from a background Bash task (started with Bash run_in_background:true), \
         check its status, or kill it.\n\
         Usage:\n\
         - Pass the task id Bash returned (e.g. \"bash_1\"). Returns only the output produced since \
         your last BashOutput call for that id, plus the status (running / exited / killed).\n\
         - Set kill:true to terminate the task (kills the whole process tree).\n\
         - Omit id and pass list:true to list all background tasks and their status.\n\
         - Poll periodically while running; once the status is exited/killed, no more output comes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Background task id from Bash (e.g. \"bash_1\")." },
                "kill": { "type": "boolean", "description": "Terminate the task (whole process tree)." },
                "list": { "type": "boolean", "description": "List all background tasks instead of reading one." }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if args.get("list").and_then(Value::as_bool).unwrap_or(false) {
            let tasks = ctx.background.list();
            if tasks.is_empty() {
                return Ok(ToolOutput::ok("No background tasks."));
            }
            let mut lines: Vec<String> = tasks
                .iter()
                .map(|(id, cmd, st)| format!("{id}  [{}]  {}", st.label(), truncate(cmd)))
                .collect();
            lines.sort();
            return Ok(ToolOutput::ok(lines.join("\n")));
        }

        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("id is required (or pass list:true)".into()))?;

        if args.get("kill").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(if ctx.background.kill(id) {
                ToolOutput::ok(format!("Killed background task `{id}`."))
            } else {
                ToolOutput::error(format!("No background task with id `{id}`."))
            });
        }

        match ctx.background.read_new(id) {
            Some((new, state)) => {
                let body = if new.trim().is_empty() {
                    "(no new output)".to_string()
                } else {
                    new
                };
                Ok(ToolOutput::ok(format!("[{}]\n{}", state.label(), body)))
            }
            None => Ok(ToolOutput::error(format!(
                "No background task with id `{id}`. Use list:true to see active tasks."
            ))),
        }
    }
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 80 {
        s.to_string()
    } else {
        let t: String = s.chars().take(77).collect();
        format!("{t}...")
    }
}
