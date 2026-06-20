//! Write: 整文件全量写 (新建或覆盖)。**新文件免读;已存在文件必先 Read + stale 检测** (§10)。
//! 原子写 (§5 第一梯队), 写后回写缓存。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::file_state::FileState;
use syncode_core::permission::{ActionClass, ActionRequest};
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

    /// 写文件 = `WriteFs`; 带绝对目标路径供审批做根内 / 根外判定 (根内放行, 根外 Ask)。
    fn classify(&self, args: &Value) -> Option<ActionRequest> {
        let mut req = ActionRequest::new(ActionClass::WriteFs, "Write");
        if let Some(p) = args.get("file_path").and_then(Value::as_str) {
            req = req.with_target(p);
        }
        Some(req)
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
        // 写收容 (P1c): 路径必须在授权写根内 (canonicalize 解析符号链接, 挡逃逸)。
        ctx.ensure_writable(&path)?;
        let exists = path.exists();

        // 已存在文件: 必先读 + stale 检测 (新文件跳过)。
        if exists {
            if let Some(err) = fsutil::check_read_and_fresh(&ctx.files, &path) {
                return Ok(ToolOutput::error(err));
            }
        }

        // 旧内容 (供 diff): 已存在文件读现盘 LF 文本; 新文件为空。
        let old = if exists {
            fsutil::read_text_lf(&path).map(|(c, _)| c).unwrap_or_default()
        } else {
            String::new()
        };
        let normalized = content.replace("\r\n", "\n"); // Write 全量替换, 输出统一 LF。
        fsutil::write_atomic(&path, &normalized)
            .map_err(|e| ToolError::Exec(format!("write failed for {file_path}: {e}")))?;

        let diff = fsutil::make_diff(file_path, &old, &normalized);
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
        // 落盘改动主动推给 LSP (若该文件已在某常驻服务器里打开)。
        ctx.lsp.notify_file_changed(&path).await;

        let msg = if exists {
            format!("The file {file_path} has been updated successfully.")
        } else {
            format!("File created successfully at: {file_path}")
        };
        let mut out = ToolOutput::ok(msg);
        if let Some(d) = diff {
            out = out.with_diff(d);
        }
        Ok(out)
    }
}
