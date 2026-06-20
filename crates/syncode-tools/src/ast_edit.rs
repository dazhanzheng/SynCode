//! AstEdit: 结构化改写 —— ast-grep pattern → rewrite, 改完 re-parse **硬保证语法合法** (§4 编辑 MED-HIGH)。
//! 与 Edit 同契约: 必先 Read + stale 检测 + 原子写 + 保留 CRLF。区别: 按语法结构替换 + 引入语法错就拒绝。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_ast::{AstError, Engine};
use syncode_core::file_state::FileState;
use syncode_core::permission::{ActionClass, ActionRequest};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct AstEditTool;

#[async_trait]
impl Tool for AstEditTool {
    fn name(&self) -> &str {
        "AstEdit"
    }

    /// 结构化改写 = `WriteFs`; 带目标路径供审批做根内 / 根外判定。
    fn classify(&self, args: &Value) -> Option<ActionRequest> {
        let mut req = ActionRequest::new(ActionClass::WriteFs, "AstEdit");
        if let Some(p) = args.get("file_path").and_then(Value::as_str) {
            req = req.with_target(p);
        }
        Some(req)
    }

    fn description(&self) -> &str {
        "Rewrite code by syntax structure: replace every node matching an ast-grep `pattern` with \
         `rewrite`, and the change is REJECTED if it would introduce a syntax error.\n\
         Usage:\n\
         - You must Read the file first; this tool errors otherwise.\n\
         - `pattern` matches like AstGrep (metavariables UPPERCASE, `$$$` for lists); reuse the same \
         metavariables in `rewrite`, e.g. pattern `let $N = $V;` → rewrite `const $N: i32 = $V;`.\n\
         - After the edit the whole file is re-parsed; if the result is not syntactically valid the \
         edit is rejected and the file is left unchanged.\n\
         - `lang` is inferred from the file extension unless given.\n\
         - For an exact-text replacement rather than a structural one, use Edit."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file to edit." },
                "pattern": { "type": "string", "description": "ast-grep pattern to match (metavars UPPERCASE; $$$ = list)." },
                "rewrite": { "type": "string", "description": "Replacement template; reuse the pattern's metavars, e.g. `const $N = $V;`." },
                "lang": { "type": "string", "description": "Language override; inferred from the file extension if omitted." }
            },
            "required": ["file_path", "pattern", "rewrite"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("pattern is required".into()))?;
        let rewrite = args
            .get("rewrite")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("rewrite is required".into()))?;
        let lang_arg = args.get("lang").and_then(Value::as_str);
        let path = PathBuf::from(file_path);
        // 写收容 (P1c): 路径必须在授权写根内 (canonicalize 解析符号链接, 挡逃逸)。
        ctx.ensure_writable(&path)?;

        let engine = match lang_arg {
            Some(name) => Engine::for_name(name)
                .map_err(|_| ToolError::InvalidArgs(format!("unknown lang: {name}")))?,
            None => match Engine::for_path(&path) {
                Ok(e) => e,
                Err(_) => {
                    return Ok(ToolOutput::error(
                        "Could not detect the language from the file extension. Pass `lang` \
                         (e.g. \"rust\").",
                    ));
                }
            },
        };

        // 必先读 + stale (与 Edit 同, §10)。
        if let Some(err) = fsutil::check_read_and_fresh(&ctx.files, &path) {
            return Ok(ToolOutput::error(err));
        }

        let (current, _) = fsutil::read_text_lf(&path)
            .map_err(|e| ToolError::Exec(format!("could not read {file_path}: {e}")))?;

        let (updated, n) = match engine.rewrite(&current, pattern, rewrite) {
            Ok(v) => v,
            Err(AstError::BadPattern(msg)) => {
                return Ok(ToolOutput::error(format!(
                    "Invalid ast-grep pattern: {msg}. Metavariables must be UPPERCASE ($NAME) or `$_`."
                )));
            }
            Err(AstError::InvalidRewrite { lang, detail }) => {
                return Ok(ToolOutput::error(format!(
                    "Rewrite rejected: it would introduce a {lang} syntax error ({detail}). \
                     Adjust the rewrite template so the result stays syntactically valid."
                )));
            }
            Err(AstError::UnknownLanguage) => {
                return Ok(ToolOutput::error("Could not detect the language; pass `lang`."));
            }
        };

        if n == 0 {
            return Ok(ToolOutput::error(
                "The pattern matched nothing in the file, so no changes were made. Check the \
                 pattern against the file's syntax (metavariables must be UPPERCASE).",
            ));
        }

        // 保留原换行风格 (缓存仍存 LF 归一版供 stale 比较) —— 与 Edit 一致。
        let to_write = if fsutil::file_is_crlf(&path) {
            updated.replace('\n', "\r\n")
        } else {
            updated.clone()
        };
        fsutil::write_atomic(&path, &to_write)
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
        // 落盘改动主动推给 LSP (若该文件已在某常驻服务器里打开)。
        ctx.lsp.notify_file_changed(&path).await;

        Ok(ToolOutput::ok(format!(
            "The file {file_path} has been updated with {n} structural replacement{} (syntax verified).",
            if n == 1 { "" } else { "s" }
        )))
    }
}
