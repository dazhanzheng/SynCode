//! Write: 整文件全量写 (新建或覆盖)。**新文件免读;已存在文件必先 Read + stale 检测** (§10)。
//! 原子写 (§5 第一梯队), 写后回写缓存。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::file_state::FileState;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating a new file or fully overwriting an existing one.\n\
         Usage:\n\
         - An existing file must be Read first; this tool errors if you overwrite an existing file \
         without reading it.\n\
         - Prefer the Edit tool to change an existing file (it only replaces the targeted text); use \
         Write only to create a new file or do a complete rewrite.\n\
         - Do not create documentation or README (*.md) files unless the user explicitly asks for them.\n\
         - file_path must be an absolute path."
    }

    fn is_dangerous(&self) -> bool {
        true // 写文件: 改系统状态
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file to write." },
                "content": { "type": "string", "description": "The full content to write to the file." }
            },
            "required": ["file_path", "content"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("content is required".into()))?;
        let path = PathBuf::from(file_path);
        let exists = path.exists();

        // 已存在文件: 必先读 + stale 检测 (新文件跳过)。
        if exists {
            if let Some(err) = fsutil::check_read_and_fresh(&ctx.files, &path) {
                return Ok(ToolOutput::error(err));
            }
        }

        let normalized = content.replace("\r\n", "\n"); // Write 全量替换, 输出统一 LF。
        fsutil::write_atomic(&path, &normalized)
            .map_err(|e| ToolError::Exec(format!("write failed for {file_path}: {e}")))?;

        // 回写缓存 (offset/limit=None: 防下次写被自己误判 stale)。
        let mtime = fsutil::mtime_ms(&path).unwrap_or(0);
        ctx.files.set(
            &path,
            FileState {
                content: normalized,
                timestamp: mtime,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );

        let msg = if exists {
            format!("The file {file_path} has been updated successfully.")
        } else {
            format!("File created successfully at: {file_path}")
        };
        Ok(ToolOutput::ok(msg))
    }
}
