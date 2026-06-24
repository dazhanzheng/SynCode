//! AstGrep: 结构化(语法)搜索 —— ast-grep pattern (`$VAR`/`$$$`) 按语法形状搜, 拿 typed match
//! + 行号 (§4 结构化搜索 HIGH)。文本 grep 表达不了的"所有 `let` 绑定 / 所有 `foo($$$)` 调用"它能搜。
//! 独立工具, 不碰 FileStateCache (与 Grep 同, §10)。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_ast::{AstError, Engine};
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct AstGrepTool;

#[async_trait]
impl Tool for AstGrepTool {
    fn name(&self) -> &str {
        "AstGrep"
    }

    fn description(&self) -> &str {
        "Search code by syntax structure using an ast-grep pattern — finds matches a text regex \
         cannot express.\n\
         Usage:\n\
         - The pattern is real code with metavariables: `$NAME` matches one node, `$$$` matches a \
         list (e.g. `println!($$$)`, `let $N = $V;`, `fn $F($$$) -> $R { $$$ }`).\n\
         - Metavariables must be UPPERCASE ($NAME) or `$_`; lowercase names are matched literally.\n\
         - Pass `lang` (e.g. \"rust\") when searching a directory; for a single file it is inferred \
         from the extension.\n\
         - Filter with the glob parameter; head_limit caps results. Output is `path:line:match`.\n\
         - For a plain-text or regex search, use Grep instead."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "ast-grep structural pattern. Metavars UPPERCASE; $$$ matches a list." },
                "path": { "type": "string", "description": "File or directory to search. Defaults to cwd." },
                "lang": { "type": "string", "description": "Language (rust/python/ts/...). Required for a directory; inferred from extension for a single file." },
                "glob": { "type": "string", "description": "Extra glob filter, e.g. *.rs" },
                "head_limit": { "type": "integer", "description": "Max number of matches to return." }
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
        let root = args
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());
        let lang_arg = args.get("lang").and_then(Value::as_str);
        let glob = args.get("glob").and_then(Value::as_str);
        let cap = args.get("head_limit").and_then(Value::as_u64).unwrap_or(200) as usize;

        // 解析引擎语言: 显式 lang 优先; 否则单文件按扩展名推断; 目录又没 lang → 要求指定。
        let engine = match lang_arg {
            Some(name) => Engine::for_name(name)
                .map_err(|_| ToolError::InvalidArgs(format!("unknown lang: {name}")))?,
            None => {
                if root.is_file() {
                    Engine::for_path(&root).map_err(|_| {
                        ToolError::InvalidArgs(
                            "could not detect language from the file extension; pass `lang`".into(),
                        )
                    })?
                } else {
                    return Err(ToolError::InvalidArgs(
                        "searching a directory requires `lang` (e.g. \"rust\")".into(),
                    ));
                }
            }
        };

        // 收集待搜文件: 单文件就它; 目录则用该语言的 file_types 过滤 (只读该语言的文件)。
        let files: Vec<PathBuf> = if root.is_file() {
            vec![root.clone()]
        } else {
            let mut b = ignore::WalkBuilder::new(&root);
            b.types(engine.lang().file_types());
            if let Some(g) = glob {
                let mut ob = ignore::overrides::OverrideBuilder::new(&root);
                ob.add(g)
                    .map_err(|e| ToolError::InvalidArgs(format!("invalid glob: {e}")))?;
                if let Ok(ov) = ob.build() {
                    b.overrides(ov);
                }
            }
            b.build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .map(|e| e.path().to_path_buf())
                .collect()
        };

        let mut out: Vec<String> = Vec::new();
        'outer: for f in &files {
            let Ok(text) = std::fs::read_to_string(f) else {
                continue;
            };
            match engine.search(&text, pattern) {
                Ok(hits) => {
                    for h in hits {
                        out.push(format!("{}:{}:{}", f.display(), h.start_line, first_line(&h.text)));
                        if out.len() >= cap {
                            break 'outer;
                        }
                    }
                }
                // pattern 在该语言下不合法 → 整体回错 (语言级问题, 不是某个文件的事)。
                Err(AstError::BadPattern(msg)) => {
                    return Ok(ToolOutput::error(format!(
                        "Invalid ast-grep pattern for {}: {msg}. Metavariables must be UPPERCASE \
                         ($NAME) or `$_`.",
                        engine.lang()
                    )));
                }
                Err(_) => continue,
            }
        }

        Ok(ToolOutput::ok(if out.is_empty() {
            "No structural matches found.".to_string()
        } else {
            out.join("\n")
        }))
    }
}

/// 多行匹配只显示首行 + 省略号, 避免一条 match 刷屏。
fn first_line(text: &str) -> String {
    match text.split_once('\n') {
        Some((head, _)) => format!("{head} …"),
        None => text.to_string(),
    }
}
