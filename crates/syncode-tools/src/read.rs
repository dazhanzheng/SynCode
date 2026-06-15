//! Read: `offset`/`limit` 行窗口 + 大文件分页 (借鉴 CC, §10)。
//! 抬上限方向: `memmap2` 大文件随机访问 (§5 第一梯队)。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{Tool, ToolError, ToolOutput};

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the local filesystem. Supports an optional line window via offset/limit."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file to read." },
                "offset": { "type": "integer", "description": "1-based line to start reading from." },
                "limit": { "type": "integer", "description": "Maximum number of lines to read." }
            },
            "required": ["file_path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> Result<ToolOutput, ToolError> {
        todo!("read file with offset/limit window (guide §10)")
    }
}
