//! Lsp: 进程内语义查询工具 (借鉴 CC LSPTool 设计 IP, §10 / §4 代码智能 HIGH ⭐⭐⭐)。
//!
//! **单工具多操作** (operation 枚举) + 统一 **1-based** filePath/line/character —— 照 CC LSPTool 的
//! 契约面。背后是 `ctx.lsp` 里**常驻**的 rust-analyzer (机制③, 复用持久语义索引), 跨调用复用同一进程。
//! 拿的是**真语义事实** (定义在哪 / 谁引用了它 / 改完的诊断), 而非对文本 grep 猜。v1 仅 Rust (.rs)。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use syncode_lsp::path_to_file_uri;

pub struct LspTool;

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "Lsp"
    }

    fn description(&self) -> &str {
        "Query a language server for code intelligence — real semantic facts a text search cannot give.\n\
         Operations:\n\
         - definition: where the symbol at a position is defined\n\
         - references: every reference to the symbol at a position\n\
         - hover: type and documentation for the symbol at a position\n\
         - documentSymbol: all symbols (functions, types, ...) declared in a file\n\
         - diagnostics: compiler/linter diagnostics for a file — use it after an edit to self-check\n\
         Usage:\n\
         - line and character are 1-based (as the editor and Read's cat -n output show them).\n\
         - definition/references/hover require line and character; documentSymbol/diagnostics need only file_path.\n\
         - v1 supports Rust (.rs) via rust-analyzer; the server starts and indexes on first use, so the \
         first index-dependent query may pause briefly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["definition", "references", "hover", "documentSymbol", "diagnostics"],
                    "description": "Which code-intelligence query to run."
                },
                "file_path": { "type": "string", "description": "Absolute path to the file." },
                "line": { "type": "integer", "description": "1-based line (required for definition/references/hover)." },
                "character": { "type": "integer", "description": "1-based column (required for definition/references/hover)." },
                "include_declaration": { "type": "boolean", "description": "references: include the declaration itself (default true)." }
            },
            "required": ["operation", "file_path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let op = args
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("operation is required".into()))?;
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let path = PathBuf::from(file_path);

        // v1: 仅 Rust。
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            return Ok(ToolOutput::error(
                "The Lsp tool currently supports only Rust (.rs) files (rust-analyzer).",
            ));
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return Ok(ToolOutput::error(format!("could not read {file_path}: {e}"))),
        };
        let uri = path_to_file_uri(&path);

        // 取/惰性启常驻 client (工程根 = cwd), 把文档当前内容同步过去。
        let client = match ctx.lsp.client_for_root(&ctx.cwd).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "could not start the language server ({e}). Is rust-analyzer installed and on PATH?"
                )));
            }
        };
        if let Err(e) = ctx.lsp.sync_doc(&client, &uri, "rust", &text).await {
            return Ok(ToolOutput::error(format!(
                "failed to sync the document to the language server: {e}"
            )));
        }

        // 需位置的操作: 取 1-based line/character, 转 LSP 的 0-based。
        let position = || -> Result<(u32, u32), String> {
            let line = args
                .get("line")
                .and_then(Value::as_u64)
                .ok_or("line is required for this operation (1-based)")?;
            let ch = args
                .get("character")
                .and_then(Value::as_u64)
                .ok_or("character is required for this operation (1-based)")?;
            Ok((line.saturating_sub(1) as u32, ch.saturating_sub(1) as u32))
        };

        let out = match op {
            "documentSymbol" => match client.document_symbol(&uri).await {
                Ok(v) => format_symbols(&v),
                Err(e) => return Ok(ToolOutput::error(format!("documentSymbol failed: {e}"))),
            },
            "diagnostics" => {
                // 诊断由服务器**异步推送**; 等索引就绪 + 给一点产出时间, 再取快照。
                client.wait_until_ready(Duration::from_secs(30)).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
                format_diagnostics(&client.diagnostics_for(&uri))
            }
            "definition" | "references" | "hover" => {
                let (line, ch) = match position() {
                    Ok(p) => p,
                    Err(e) => return Ok(ToolOutput::error(e)),
                };
                // 这些**需索引**: 等就绪 (超时也继续, 拿到啥算啥)。
                client.wait_until_ready(Duration::from_secs(30)).await;
                match op {
                    "definition" => match client.definition(&uri, line, ch).await {
                        Ok(v) => format_locations(&v),
                        Err(e) => return Ok(ToolOutput::error(format!("definition failed: {e}"))),
                    },
                    "references" => {
                        let incl = args
                            .get("include_declaration")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        match client.references(&uri, line, ch, incl).await {
                            Ok(v) => format_locations(&v),
                            Err(e) => return Ok(ToolOutput::error(format!("references failed: {e}"))),
                        }
                    }
                    _ => match client.hover(&uri, line, ch).await {
                        Ok(v) => format_hover(&v),
                        Err(e) => return Ok(ToolOutput::error(format!("hover failed: {e}"))),
                    },
                }
            }
            other => return Ok(ToolOutput::error(format!("unknown operation: {other}"))),
        };

        Ok(ToolOutput::ok(if out.trim().is_empty() {
            "No results.".to_string()
        } else {
            out
        }))
    }
}

// ---- LSP JSON 结果 → 写给模型读的文本 ----

/// `file://` URI → 本地路径 (Windows `/C:/..`→`C:/..`, Unix 保留前导 `/`)。
fn uri_to_path(uri: &str) -> String {
    let s = uri.strip_prefix("file://").unwrap_or(uri);
    if let Some(rest) = s.strip_prefix('/') {
        if rest.len() >= 2 && &rest[1..2] == ":" {
            return rest.to_string(); // Windows 盘符
        }
    }
    s.to_string()
}

/// range.start → (1-based line, 1-based col)。
fn range_start_1based(range: &Value) -> Option<(u64, u64)> {
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? + 1;
    let ch = start.get("character")?.as_u64()? + 1;
    Some((line, ch))
}

/// Location / Location[] / LocationLink[] → 每行 `path:line:col`。
fn format_locations(v: &Value) -> String {
    let mut out = Vec::new();
    collect_locations(v, &mut out);
    out.join("\n")
}

fn collect_locations(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(arr) => {
            for item in arr {
                collect_locations(item, out);
            }
        }
        Value::Object(o) => {
            let uri = o
                .get("uri")
                .or_else(|| o.get("targetUri"))
                .and_then(Value::as_str);
            let range = o
                .get("range")
                .or_else(|| o.get("targetSelectionRange"))
                .or_else(|| o.get("targetRange"));
            if let (Some(uri), Some(range)) = (uri, range) {
                if let Some((l, c)) = range_start_1based(range) {
                    out.push(format!("{}:{}:{}", uri_to_path(uri), l, c));
                }
            }
        }
        _ => {}
    }
}

/// DocumentSymbol[] (层级) 或 SymbolInformation[] (扁平) → 缩进的 `kind name (line N)`。
fn format_symbols(v: &Value) -> String {
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for s in arr {
            collect_symbol(s, 0, &mut out);
        }
    }
    out.join("\n")
}

fn collect_symbol(s: &Value, depth: usize, out: &mut Vec<String>) {
    let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
    let range = s
        .get("range")
        .or_else(|| s.get("location").and_then(|l| l.get("range")));
    let line = range.and_then(range_start_1based).map(|(l, _)| l).unwrap_or(0);
    let kind = s
        .get("kind")
        .and_then(Value::as_u64)
        .map(symbol_kind_name)
        .unwrap_or("");
    out.push(format!("{}{kind} {name} (line {line})", "  ".repeat(depth)));
    if let Some(children) = s.get("children").and_then(Value::as_array) {
        for c in children {
            collect_symbol(c, depth + 1, out);
        }
    }
}

/// Hover.contents (MarkupContent / MarkedString / 其数组) → 纯文本。
fn format_hover(v: &Value) -> String {
    match v.get("contents") {
        Some(Value::Object(o)) => o.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| {
                x.get("value")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| x.as_str().map(String::from))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Diagnostic[] → 每行 `line:col severity: message`。
fn format_diagnostics(diags: &[Value]) -> String {
    if diags.is_empty() {
        return "No diagnostics.".to_string();
    }
    diags
        .iter()
        .filter_map(|d| {
            let (l, c) = d.get("range").and_then(range_start_1based)?;
            let sev = d
                .get("severity")
                .and_then(Value::as_u64)
                .map(severity_name)
                .unwrap_or("note");
            let msg = d.get("message").and_then(Value::as_str).unwrap_or("");
            Some(format!("{l}:{c} {sev}: {msg}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// LSP `SymbolKind` 数值 → 名称 (常用项, 其余空)。
fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        2 => "module",
        5 => "class",
        6 => "method",
        8 => "field",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        22 => "enum-member",
        23 => "struct",
        26 => "type-param",
        _ => "symbol",
    }
}

/// LSP `DiagnosticSeverity` 数值 → 名称。
fn severity_name(sev: u64) -> &'static str {
    match sev {
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "hint",
    }
}
