//! Edit: `old_string` 唯一匹配替换 + `replace_all` 开关 (借鉴 CC, §10)。
//! **必先 Read + stale 检测**, 原子写, 写后回写缓存。抬上限方向: AST 级改写 (tree-sitter / ast-grep)。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::file_state::FileState;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace an exact, unique occurrence of old_string with new_string in a file. \
         Set replace_all to replace every occurrence. You must Read the file first."
    }

    fn is_dangerous(&self) -> bool {
        true
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file to edit." },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique unless replace_all)." },
                "new_string": { "type": "string", "description": "Replacement text (must differ from old_string)." },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences instead of requiring uniqueness." }
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let old_string = args
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("old_string is required".into()))?;
        let new_string = args
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("new_string is required".into()))?;
        let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        let path = PathBuf::from(file_path);

        if old_string == new_string {
            return Ok(ToolOutput::error(
                "No changes to make: old_string and new_string are exactly the same.",
            ));
        }
        // 必先读 + stale。
        if let Some(err) = fsutil::check_read_and_fresh(&ctx.files, &path) {
            return Ok(ToolOutput::error(err));
        }

        let (current, _) = fsutil::read_text_lf(&path)
            .map_err(|e| ToolError::Exec(format!("could not read {file_path}: {e}")))?;

        let n = current.matches(old_string).count();
        if n == 0 {
            return Ok(ToolOutput::error("String to replace not found in file."));
        }
        if n > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "Found {n} matches of the string to replace, but replace_all is false. \
                 To replace all occurrences, set replace_all to true. To replace only one occurrence, \
                 please provide more context to uniquely identify the instance."
            )));
        }

        let updated = if replace_all {
            current.replace(old_string, new_string)
        } else {
            current.replacen(old_string, new_string, 1)
        };
        fsutil::write_atomic(&path, &updated)
            .map_err(|e| ToolError::Exec(format!("write failed for {file_path}: {e}")))?;

        let mtime = fsutil::mtime_ms(&path).unwrap_or(0);
        ctx.files.set(
            &path,
            FileState {
                content: updated,
                timestamp: mtime,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        Ok(ToolOutput::ok(format!(
            "The file {file_path} has been updated successfully."
        )))
    }
}
