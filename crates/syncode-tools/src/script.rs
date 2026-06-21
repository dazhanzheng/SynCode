//! Script: **code-as-action** (支柱 3) —— 把多步文件操作压成一段**进程内** Rhai 脚本, 一次工具调用
//! 完成 N 步, 省掉 N-1 个 LLM 往返 (也间接缓解支柱 1: 更少轮 = context 增长更慢)。
//!
//! **能力面 = 绑定的 host 函数, 别无其它** (这正是嵌入式 VM 做 code-as-action 的安全红利):
//! Rhai 本身无任何 ambient 能力 (碰不到 fs / 网络 / 进程), 脚本只能调我们显式绑定的 `read`/`write`/
//! `edit`/`replace_all`/`exists`。其中**写类一律走 `FsScope` 的 cap-std 收容** —— 物理上逃不出授权根
//! (与 Write/Edit 工具同一条边界, 见 [`fsutil::write_contained`])。故脚本即便写循环也越不出工作区。
//!
//! 安全分类: `WriteFs` 无具体 target → autonomy-first 放行 (项目内); 真边界是 cap-std 收容, 不是分类器
//! (纲领: 分类器是 UX 层, 沙箱才是边界)。
//!
//! v1 绑定: read / write / edit / replace_all / exists + `print`(收进输出)。LSP 通知在脚本跑完后补发;
//! AST/grep/glob/lsp 的脚本绑定与多文件 diff 入 UI 留作后续 (本版聚焦最高价值的「批量读改」闭环)。

use crate::fsutil;
use async_trait::async_trait;
use rhai::{Dynamic, Engine};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use syncode_core::file_state::{FileState, FileStateCache};
use syncode_core::fs_scope::SharedFsScope;
use syncode_core::permission::{ActionClass, ActionRequest};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct ScriptTool;

/// Rhai host 函数共享的运行态 (脚本闭包跨调用共享): 输出缓冲 + 本次改动的文件列表。
#[derive(Default)]
struct ScriptState {
    output: Vec<String>,
    changed: Vec<PathBuf>,
}

#[async_trait]
impl Tool for ScriptTool {
    fn name(&self) -> &str {
        "Script"
    }

    fn description(&self) -> &str {
        "Run a short Rhai script in-process to perform several file operations in one call, instead \
         of issuing many separate tool calls. Use this when a task needs a batch of reads/edits that \
         follow a simple pattern (e.g. apply the same edit across files you already know, or read a \
         file then rewrite part of it) — it saves round-trips.\n\
         Bound functions (all confined to the workspace; absolute or cwd-relative paths):\n\
         - read(path) -> string : file contents (LF-normalized). Records the read so a later edit/write succeeds.\n\
         - write(path, content) : create or overwrite a file (contained to the workspace; errors outside it).\n\
         - edit(path, old, new) : replace the single unique occurrence of old with new (errors if missing or ambiguous).\n\
         - replace_all(path, old, new) : replace every occurrence.\n\
         - exists(path) -> bool\n\
         - print(x) : add x to the output returned to you.\n\
         Rhai is an ordinary scripting language (let, if, for, fn, arrays, maps). The script's printed \
         output and return value come back to you. Writes cannot escape the workspace. Prefer the plain \
         Read/Edit/Write tools for a single operation; reach for Script only to batch several."
    }

    /// 脚本可写文件 → `WriteFs` (无 target: autonomy-first 项目内放行)。真正的出根拦截靠 cap-std 收容。
    fn classify(&self, _args: &Value) -> Option<ActionRequest> {
        Some(ActionRequest::new(ActionClass::WriteFs, "Script"))
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": { "type": "string", "description": "The Rhai script to run." }
            },
            "required": ["script"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let script = args
            .get("script")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("script is required".into()))?
            .to_string();

        let state = Arc::new(Mutex::new(ScriptState::default()));

        // 脚本同步执行 (Rhai 无 async)。Engine 与 `Dynamic` 都是 !Send, 故整段在一个**不跨 await** 的块里
        // 跑完, 块内就把返回值收敛成 `Option<String>` (别让任何 rhai 类型逃出块/跨 await), 之后再 await
        // LSP 通知 —— 保证 async_trait 要求的 Send future 成立。
        let eval_result: Result<Option<String>, String> = {
            let engine = build_engine(&ctx.cwd, ctx.files.clone(), ctx.fs.clone(), state.clone());
            match engine.eval::<Dynamic>(&script) {
                Ok(v) => Ok(if v.is_unit() { None } else { Some(v.to_string()) }),
                Err(e) => Err(e.to_string()),
            }
        };

        // 脚本跑完后补发 LSP 变更通知 (脚本内不能 await)。
        let changed = std::mem::take(&mut state.lock().unwrap().changed);
        for p in &changed {
            ctx.lsp.notify_file_changed(p).await;
        }

        let mut out = std::mem::take(&mut state.lock().unwrap().output);
        match eval_result {
            Ok(value) => {
                if let Some(v) = value {
                    out.push(format!("=> {v}"));
                }
                if !changed.is_empty() {
                    let names: Vec<String> =
                        changed.iter().map(|p| p.display().to_string()).collect();
                    out.push(format!("[{} file(s) changed: {}]", changed.len(), names.join(", ")));
                }
                let text = if out.is_empty() {
                    "Script completed with no output.".to_string()
                } else {
                    out.join("\n")
                };
                Ok(ToolOutput::ok(text))
            }
            Err(err) => {
                // 已产出的输出也回带, 利于模型定位脚本在哪一步炸的。
                let mut msg = format!("Script error: {err}");
                if !out.is_empty() {
                    msg.push_str("\n--- output before the error ---\n");
                    msg.push_str(&out.join("\n"));
                }
                Ok(ToolOutput::error(msg))
            }
        }
    }
}

/// 构建一个只绑定**受收容文件 host 函数**的 Rhai 引擎。`print` 收进输出缓冲; 写类走 `FsScope` 收容。
fn build_engine(
    cwd: &Path,
    files: Arc<FileStateCache>,
    fs: SharedFsScope,
    state: Arc<Mutex<ScriptState>>,
) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(5_000_000); // 防失控循环
    engine.set_max_string_size(50 * 1024 * 1024);
    engine.set_max_call_levels(64);

    // print(x) / 脚本里的 print → 收进输出缓冲。
    {
        let state = state.clone();
        engine.on_print(move |s| state.lock().unwrap().output.push(s.to_string()));
    }

    let cwd = cwd.to_path_buf();

    // read(path) -> string : 读 LF 文本, 并登记到 FileStateCache (使随后的 edit/write 通过「必先读」)。
    {
        let cwd = cwd.clone();
        let files = files.clone();
        engine.register_fn("read", move |path: &str| -> Result<String, Box<rhai::EvalAltResult>> {
            let abs = resolve(&cwd, path);
            let (content, mtime) = fsutil::read_text_lf(&abs)
                .map_err(|e| rt(format!("read failed for {path}: {e}")))?;
            files.set(
                &abs,
                FileState {
                    content: content.clone(),
                    timestamp: mtime,
                    offset: None,
                    limit: None,
                    is_partial_view: false,
                },
            );
            Ok(content)
        });
    }

    // exists(path) -> bool
    {
        let cwd = cwd.clone();
        engine.register_fn("exists", move |path: &str| resolve(&cwd, path).exists());
    }

    // write(path, content) : 收容式整文件写。
    {
        let cwd = cwd.clone();
        let files = files.clone();
        let fs = fs.clone();
        let state = state.clone();
        engine.register_fn(
            "write",
            move |path: &str, content: &str| -> Result<(), Box<rhai::EvalAltResult>> {
                let abs = resolve(&cwd, path);
                let normalized = content.replace("\r\n", "\n");
                fsutil::write_contained(&fs, &abs, &normalized)
                    .map_err(|e| rt(format!("write failed for {path}: {e}")))?;
                record_write(&files, &state, &abs, normalized);
                Ok(())
            },
        );
    }

    // edit(path, old, new) : 唯一匹配替换。
    {
        let cwd = cwd.clone();
        let files = files.clone();
        let fs = fs.clone();
        let state = state.clone();
        engine.register_fn(
            "edit",
            move |path: &str, old: &str, new: &str| -> Result<(), Box<rhai::EvalAltResult>> {
                edit_impl(&cwd, &files, &fs, &state, path, old, new, false)
            },
        );
    }

    // replace_all(path, old, new) : 全部替换。
    {
        let cwd = cwd.clone();
        let files = files.clone();
        let fs = fs.clone();
        let state = state.clone();
        engine.register_fn(
            "replace_all",
            move |path: &str, old: &str, new: &str| -> Result<(), Box<rhai::EvalAltResult>> {
                edit_impl(&cwd, &files, &fs, &state, path, old, new, true)
            },
        );
    }

    engine
}

#[allow(clippy::too_many_arguments)]
fn edit_impl(
    cwd: &Path,
    files: &Arc<FileStateCache>,
    fs: &SharedFsScope,
    state: &Arc<Mutex<ScriptState>>,
    path: &str,
    old: &str,
    new: &str,
    all: bool,
) -> Result<(), Box<rhai::EvalAltResult>> {
    if old == new {
        return Err(rt("edit: old and new are identical".into()));
    }
    let abs = resolve(cwd, path);
    let (current, _) =
        fsutil::read_text_lf(&abs).map_err(|e| rt(format!("edit: could not read {path}: {e}")))?;
    let n = current.matches(old).count();
    if n == 0 {
        return Err(rt(format!("edit: string to replace not found in {path}")));
    }
    if n > 1 && !all {
        return Err(rt(format!(
            "edit: found {n} matches in {path}; pass more context to make it unique, or use replace_all"
        )));
    }
    let updated = if all { current.replace(old, new) } else { current.replacen(old, new, 1) };
    // 保留原换行风格 (与 Edit 工具一致)。
    let to_write =
        if fsutil::file_is_crlf(&abs) { updated.replace('\n', "\r\n") } else { updated.clone() };
    fsutil::write_contained(fs, &abs, &to_write)
        .map_err(|e| rt(format!("edit: write failed for {path}: {e}")))?;
    record_write(files, state, &abs, updated);
    Ok(())
}

/// 写后: 更新 FileStateCache (LF 归一内容) + 记录改动文件 (供脚本结束后补 LSP 通知)。
fn record_write(
    files: &Arc<FileStateCache>,
    state: &Arc<Mutex<ScriptState>>,
    abs: &Path,
    content_lf: String,
) {
    let mtime = fsutil::mtime_ms(abs).unwrap_or(0);
    files.set(
        abs,
        FileState {
            content: content_lf,
            timestamp: mtime,
            offset: None,
            limit: None,
            is_partial_view: false,
        },
    );
    let mut st = state.lock().unwrap();
    if !st.changed.iter().any(|p| p == abs) {
        st.changed.push(abs.to_path_buf());
    }
}

/// 绝对路径直用; 相对路径相对 `cwd` 解析。
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// 造一个 Rhai 运行时错误 (host 函数失败回给脚本 / 模型)。
fn rt(msg: String) -> Box<rhai::EvalAltResult> {
    msg.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use syncode_core::fs_scope::FsScope;

    fn ctx_in(root: &Path) -> ToolCtx {
        let mut c = ToolCtx::new(Arc::new(FileStateCache::new()), root.to_path_buf());
        c.fs = Some(Arc::new(FsScope::new(root)));
        c
    }

    #[tokio::test]
    async fn script_reads_and_edits_in_one_call() {
        let root = std::env::temp_dir().join("syncode_script_test_root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "hello world").unwrap();
        let ctx = ctx_in(&root);

        let script = format!(
            r#"
            let p = "{}";
            let c = read(p);
            edit(p, "world", "rhai");
            print("len=" + c.len());
            "done"
            "#,
            root.join("a.txt").display().to_string().replace('\\', "/")
        );
        let out = ScriptTool
            .call(json!({ "script": script }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "script should succeed: {}", out.content);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello rhai");
        assert!(out.content.contains("len=11"), "got: {}", out.content);
        assert!(out.content.contains("file(s) changed"), "got: {}", out.content);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn script_write_outside_workspace_is_denied() {
        let root = std::env::temp_dir().join("syncode_script_deny_root");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ctx_in(&root);
        // 试图写到 temp 之上 (出所有写根) → cap-std 收容拒, 脚本报错, 不落盘。
        let target = std::env::temp_dir()
            .parent()
            .unwrap()
            .join("syncode_script_ESCAPE.txt");
        let script = format!(
            r#"write("{}", "evil")"#,
            target.display().to_string().replace('\\', "/")
        );
        let out = ScriptTool.call(json!({ "script": script }), &ctx).await.unwrap();
        assert!(out.is_error, "out-of-workspace write must fail: {}", out.content);
        assert!(!target.exists(), "denied write must not touch disk");
    }

    #[tokio::test]
    async fn script_syntax_error_is_reported() {
        let root = std::env::temp_dir().join("syncode_script_syntax_root");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ctx_in(&root);
        let out = ScriptTool
            .call(json!({ "script": "let x = ;" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("Script error"), "got: {}", out.content);
    }
}
