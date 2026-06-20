//! Edit: `old_string` 唯一匹配替换 + `replace_all` 开关 (借鉴 CC, §10)。
//! **必先 Read + stale 检测**, 原子写, 写后回写缓存。抬上限方向: AST 级改写 (tree-sitter / ast-grep)。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_ast::Engine;
use syncode_core::file_state::FileState;
use syncode_core::permission::{ActionClass, ActionRequest};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Perform an exact string replacement in a file: replace a single unique occurrence of \
         old_string with new_string, or set replace_all to replace every occurrence.\n\
         Usage:\n\
         - You must Read the file at least once before editing; this tool errors otherwise.\n\
         - Read shows lines in `cat -n` format (line number + tab); the file itself has no such \
         prefix, so old_string/new_string must contain only the real file text, never the line-number prefix.\n\
         - Preserve the exact whitespace and indentation of the text as it appears in the file.\n\
         - The edit FAILS if old_string is not unique: add more surrounding context to make it \
         unique, or set replace_all to change every instance (e.g. to rename a variable).\n\
         - Prefer the smallest old_string that is unambiguous (usually 2-4 adjacent lines).\n\
         - For a change defined by code structure rather than exact text, use AstEdit instead."
    }

    /// 改文件 = `WriteFs`; 带目标路径供审批做根内 / 根外判定。
    fn classify(&self, args: &Value) -> Option<ActionRequest> {
        let mut req = ActionRequest::new(ActionClass::WriteFs, "Edit");
        if let Some(p) = args.get("file_path").and_then(Value::as_str) {
            req = req.with_target(p);
        }
        Some(req)
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
        // 写收容 (P1c): 路径必须在授权写根内 (canonicalize 解析符号链接, 挡逃逸)。
        ctx.ensure_writable(&path)?;

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
        // 保留原文件换行风格: 原本 CRLF → 写回 CRLF (缓存仍存 LF 归一版供 stale 比较)。
        let to_write = if fsutil::file_is_crlf(&path) {
            updated.replace('\n', "\r\n")
        } else {
            updated.clone()
        };
        fsutil::write_atomic(&path, &to_write)
            .map_err(|e| ToolError::Exec(format!("write failed for {file_path}: {e}")))?;

        let mtime = fsutil::mtime_ms(&path).unwrap_or(0);

        // 改后语法护栏 (已知语言): **不阻断** (允许分步重构途中暂时破语法), 但把"这次改动弄坏了语法"
        // 作为 affordance 提示模型自纠 (§10 error-message-as-affordance)。硬保证合法走 AstEdit。
        let mut msg = format!("The file {file_path} has been updated successfully.");
        if let Ok(engine) = Engine::for_path(&path) {
            if let Some(detail) = engine.introduced_syntax_error(&current, &updated) {
                msg.push_str(&format!(
                    " ⚠️ Note: this edit appears to introduce a syntax error ({detail}). \
                     If that was not intended, fix it in a follow-up edit (or use AstEdit for a \
                     syntax-guaranteed change)."
                ));
            }
        }

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
        // 落盘改动主动推给 LSP (若该文件已在某常驻服务器里打开), 保持索引与编辑同步。
        ctx.lsp.notify_file_changed(&path).await;
        Ok(ToolOutput::ok(msg))
    }
}
