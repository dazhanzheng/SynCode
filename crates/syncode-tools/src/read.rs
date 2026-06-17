//! Read: `offset`/`limit` 行窗口 (借鉴 CC, §10)。成功后把全文写入共享 `FileStateCache`,
//! 供 Edit/Write 做「必先读」+ stale 检测。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::file_state::FileState;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

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

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let path = PathBuf::from(file_path);
        let offset = args.get("offset").and_then(Value::as_u64);
        let limit = args.get("limit").and_then(Value::as_u64);

        if path.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{file_path} is a directory, not a file"
            )));
        }
        let (content, mtime) = fsutil::read_text_lf(&path)
            .map_err(|e| ToolError::Exec(format!("could not read {file_path}: {e}")))?;

        let view = apply_window(&content, offset, limit);

        // 写缓存: 永远存**全文** content (stale 内容兜底比较用), 记录本次窗口 offset/limit。
        ctx.files.set(
            &path,
            FileState {
                content,
                timestamp: mtime,
                offset,
                limit,
                is_partial_view: false,
            },
        );
        Ok(ToolOutput::ok(view))
    }
}

/// 取行窗口 (1-based offset, limit 行)。无 offset/limit 则返回全文。
fn apply_window(content: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    if offset.is_none() && limit.is_none() {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = (offset.unwrap_or(1).max(1) as usize) - 1;
    let end = match limit {
        Some(l) => (start + l as usize).min(lines.len()),
        None => lines.len(),
    };
    lines.get(start..end).map(|s| s.join("\n")).unwrap_or_default()
}
