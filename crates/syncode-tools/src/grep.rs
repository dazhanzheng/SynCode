//! Grep: 接口抄 CC (`--type`/`--glob`/`-A/-B/-C`/`output_mode`/`head_limit`),
//! 实现改成**进程内调 `grep`/`ignore` 库** (省 spawn + 解析 stdout, 拿 typed match, §10/§4)。
//! 抬上限方向: `ast-grep` 结构化搜索 (HIGH, §4)。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{Tool, ToolError, ToolOutput};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression (ripgrep engine, in-process). \
         Filter by glob/type; show context lines; choose content/files/count output."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "File or directory to search in. Defaults to cwd." },
                "glob": { "type": "string", "description": "Glob filter, e.g. *.rs" },
                "type": { "type": "string", "description": "File type filter, e.g. rust" },
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"] },
                "context": { "type": "integer", "description": "Lines of context around each match (-C)." },
                "head_limit": { "type": "integer", "description": "Limit number of results." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> Result<ToolOutput, ToolError> {
        todo!("in-process ripgrep via `grep`/`ignore` crates (guide §4/§10)")
    }
}
