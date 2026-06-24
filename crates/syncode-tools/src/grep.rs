//! Grep: 接口抄 CC (`glob`/`output_mode`/`head_limit`), 实现**进程内调 `ignore`(gitignore-aware
//! 并行遍历)+ `regex`** —— 省 spawn、拿 typed match (§10/§4)。独立工具, 不碰 FileStateCache。
//! 抬上限方向: `grep-searcher` 快引擎 + `ast-grep` 结构化搜索 (§4)。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression, in-process and gitignore-aware.\n\
         Usage:\n\
         - Prefer this over running `grep` or `rg` through Bash.\n\
         - Full regex syntax via the Rust `regex` engine; escape literal regex metacharacters \
         (e.g. `\\{`, `\\(`). Each pattern is matched within a single line.\n\
         - Filter files with the glob parameter (e.g. \"*.rs\").\n\
         - output_mode: \"content\" shows matching lines, \"files_with_matches\" shows file paths \
         (default), \"count\" shows per-file match counts; head_limit caps the number of results.\n\
         - To search by code structure instead of text, use AstGrep."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "File or directory to search in. Defaults to cwd." },
                "glob": { "type": "string", "description": "Glob filter, e.g. *.rs" },
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"] },
                "head_limit": { "type": "integer", "description": "Limit number of results." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("pattern is required".into()))?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
        let root = args
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());
        let glob = args.get("glob").and_then(Value::as_str);
        let mode = args.get("output_mode").and_then(Value::as_str).unwrap_or("files_with_matches");
        let cap = args.get("head_limit").and_then(Value::as_u64).unwrap_or(500) as usize;

        let mut builder = ignore::WalkBuilder::new(&root);
        if let Some(g) = glob {
            let mut ob = ignore::overrides::OverrideBuilder::new(&root);
            ob.add(g).map_err(|e| ToolError::InvalidArgs(format!("invalid glob: {e}")))?;
            if let Ok(ov) = ob.build() {
                builder.overrides(ov);
            }
        }

        let mut files: Vec<String> = Vec::new();
        let mut content: Vec<String> = Vec::new();
        let mut counts: Vec<String> = Vec::new();

        'walk: for entry in builder.build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry.path();
            let Ok(text) = std::fs::read_to_string(p) else { continue };
            let mut file_hits = 0usize;
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    file_hits += 1;
                    if mode == "content" {
                        content.push(format!("{}:{}:{}", p.display(), i + 1, line));
                        if content.len() >= cap {
                            break 'walk;
                        }
                    }
                }
            }
            if file_hits > 0 {
                match mode {
                    "files_with_matches" => {
                        files.push(p.display().to_string());
                        if files.len() >= cap {
                            break;
                        }
                    }
                    "count" => {
                        counts.push(format!("{}:{}", p.display(), file_hits));
                        if counts.len() >= cap {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }

        let out = match mode {
            "content" => content.join("\n"),
            "count" => counts.join("\n"),
            _ => files.join("\n"),
        };
        Ok(ToolOutput::ok(if out.is_empty() {
            "No matches found.".to_string()
        } else {
            out
        }))
    }
}
