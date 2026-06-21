//! Glob: 进程内、gitignore-aware 的文件列举 / 通配 (§4 in-process 语义)。
//!
//! 列目录 / 找文件不再退回 Bash (`ls`/`dir`/`Get-ChildItem`/`find`) —— 走 `ignore`
//! (尊重 `.gitignore`、跳 `.git`/隐藏), 只读、**不过审批**。补齐「探索代码结构」闭环里最后一块
//! (列目录), 之后探索可全程 Read/Grep/AstGrep/Lsp/Glob, 不依赖 shell。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct GlobTool;

/// 默认返回上限 (条路径)。`**/*` 在大仓上可能很多, 截断并提示收窄。
const DEFAULT_CAP: usize = 1000;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "List files in the workspace, in-process and gitignore-aware (no shell).\n\
         Usage:\n\
         - Prefer this over `ls` / `dir` / `Get-ChildItem` / `find` through Bash to list files or \
         explore the directory tree.\n\
         - Optional `pattern` is a gitignore-style glob (e.g. \"**/*.rs\", \"src/**\", \"*.toml\"); a \
         bare name like \"*.rs\" matches at any depth. With no pattern, lists every non-ignored file.\n\
         - Optional `path` sets the base directory (defaults to the workspace root).\n\
         - Respects .gitignore and skips .git and hidden files; returns absolute paths, one per line, \
         sorted; capped at head_limit (default 1000).\n\
         - To search file contents, use Grep; to search by code structure, use AstGrep."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Gitignore-style glob, e.g. **/*.rs (matches at any depth). Omit to list all files." },
                "path": { "type": "string", "description": "Base directory to list from. Defaults to the workspace root." },
                "head_limit": { "type": "integer", "description": "Max paths to return (default 1000)." }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let root = args
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());
        let pattern = args.get("pattern").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
        let cap = args
            .get("head_limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_CAP);

        let mut builder = ignore::WalkBuilder::new(&root);
        // 非 git 仓库的 workspace 也尊重 .gitignore (用户可任选目录, 不一定是 git repo)。
        builder.require_git(false);
        // 通配经 `ignore` 的 override (gitignore 风格白名单): 命中的文件才产出。无 pattern → 全列。
        if let Some(g) = pattern {
            let mut ob = ignore::overrides::OverrideBuilder::new(&root);
            ob.add(g).map_err(|e| ToolError::InvalidArgs(format!("invalid glob: {e}")))?;
            let ov = ob.build().map_err(|e| ToolError::InvalidArgs(format!("invalid glob: {e}")))?;
            builder.overrides(ov);
        }

        let mut files: Vec<String> = Vec::new();
        let mut truncated = false;
        // 内存有界: 边走边截 (workspace 可能是个大目录), 不先全收集。
        for entry in builder.build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if files.len() >= cap {
                truncated = true;
                break;
            }
            files.push(entry.path().display().to_string());
        }
        files.sort();

        if files.is_empty() {
            return Ok(ToolOutput::ok("No files found.".to_string()));
        }
        let mut out = files.join("\n");
        if truncated {
            out.push_str(&format!(
                "\n[truncated at {cap} files; narrow with a more specific pattern or a deeper path]"
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}
