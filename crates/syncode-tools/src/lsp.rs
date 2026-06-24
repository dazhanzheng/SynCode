//! Lsp: 进程内语义查询工具 (借鉴 CC LSPTool 设计 IP, §10 / §4 代码智能 HIGH ⭐⭐⭐)。
//!
//! **单工具多操作** (operation 枚举) + 统一 **1-based** filePath/line/character —— 照 CC LSPTool 的
//! 契约面。背后是 `ctx.lsp` 里**常驻**的 rust-analyzer (机制③, 复用持久语义索引), 跨调用复用同一进程。
//! 拿的是**真语义事实** (定义在哪 / 谁引用了它 / 改完的诊断), 而非对文本 grep 猜。v1 仅 Rust (.rs)。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use syncode_lsp::lang::{lang_for_extension, lang_for_path, workspace_root_for};
use syncode_lsp::{path_to_file_uri, uri_to_path};

/// 索引就绪等待上限 (serverStatus 正常时通常秒级返回; 这是上限不是常态)。
const READY_TIMEOUT: Duration = Duration::from_secs(30);

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
         - implementation: implementations of the trait/method at a position\n\
         - typeDefinition: the type of the symbol at a position\n\
         - hover: type and documentation for the symbol at a position\n\
         - documentSymbol: all symbols (functions, types, ...) declared in a file\n\
         - workspaceSymbol: find symbols across the whole project by name (give `query`)\n\
         - diagnostics: compiler/linter diagnostics for a file — use it after an edit to self-check\n\
         Usage:\n\
         - Typical flow: don't know the exact spot? use workspaceSymbol (by name) or documentSymbol \
         to locate a line:col, then definition/references/hover at that position.\n\
         - line and character are 1-based (as the editor and Read's cat -n output show them).\n\
         - position ops (definition/references/implementation/typeDefinition/hover) require line and \
         character; documentSymbol/diagnostics need only file_path; workspaceSymbol needs `query`.\n\
         - An empty result for a position op usually means the cursor is not on a symbol — recheck \
         line/character. If the tool says the server is still indexing, just retry.\n\
         - Language is chosen by file extension: Rust (rust-analyzer), Go (gopls), Python (pyright), \
         TypeScript/JavaScript (typescript-language-server), C/C++ (clangd). The matching server must \
         be installed and on PATH; it starts and indexes on first use."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["definition", "references", "implementation", "typeDefinition", "hover", "documentSymbol", "workspaceSymbol", "diagnostics"],
                    "description": "Which code-intelligence query to run."
                },
                "file_path": { "type": "string", "description": "Absolute path to the file (required except for workspaceSymbol, where it scopes the project)." },
                "line": { "type": "integer", "description": "1-based line (required for position ops)." },
                "character": { "type": "integer", "description": "1-based column (required for position ops)." },
                "query": { "type": "string", "description": "Symbol name to search for (workspaceSymbol only)." },
                "include_declaration": { "type": "boolean", "description": "references: include the declaration itself (default true)." }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let op = args
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("operation is required".into()))?;

        // workspaceSymbol: 按名字找符号, 不绑定具体文件位置。
        if op == "workspaceSymbol" {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("query is required for workspaceSymbol".into()))?;
            // 语言: 有 file_path 用它的扩展名定; 没有则默认 Rust (向后兼容)。
            let (lang, root) = match args.get("file_path").and_then(Value::as_str) {
                Some(fp) => {
                    let p = Path::new(fp);
                    match lang_for_path(p) {
                        Some(l) => (l, workspace_root_for(p, &l)),
                        None => return Ok(unsupported_ext(p)),
                    }
                }
                None => (lang_for_extension("rs").unwrap(), ctx.cwd.clone()),
            };
            let client = match ctx.lsp.client_for(&root, lang.server_cmd, lang.server_args).await {
                Ok(c) => c,
                Err(e) => return Ok(server_unavailable(lang.server_cmd, &e.to_string())),
            };
            if !client.wait_until_ready(READY_TIMEOUT).await {
                return Ok(not_ready(client.is_dead()));
            }
            return Ok(match client.workspace_symbol(query).await {
                Ok(v) => ok_or_empty(format_workspace_symbols(&v), "No project symbols matched the query."),
                Err(e) => ToolOutput::error(format!("workspaceSymbol failed: {e}")),
            });
        }

        // 其余操作: 需要一个具体文件。
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let path = PathBuf::from(file_path);

        let lang = match lang_for_path(&path) {
            Some(l) => l,
            None => return Ok(unsupported_ext(&path)),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return Ok(ToolOutput::error(format!("could not read {file_path}: {e}"))),
        };
        let uri = path_to_file_uri(&path);
        let root = workspace_root_for(&path, &lang);

        let client = match ctx.lsp.client_for(&root, lang.server_cmd, lang.server_args).await {
            Ok(c) => c,
            Err(e) => return Ok(server_unavailable(lang.server_cmd, &e.to_string())),
        };
        if let Err(e) = client.sync(&uri, lang.language_id, &text).await {
            return Ok(ToolOutput::error(format!(
                "failed to sync the document to the language server: {e}"
            )));
        }

        // documentSymbol 是语法级查询, 不必等索引; 其余需索引就绪。
        let needs_index = op != "documentSymbol";
        if needs_index && !client.wait_until_ready(READY_TIMEOUT).await {
            return Ok(not_ready(client.is_dead()));
        }

        let result = match op {
            "documentSymbol" => match client.document_symbol(&uri).await {
                Ok(v) => ok_or_empty(format_symbols(&v), "No symbols found in this file."),
                Err(e) => ToolOutput::error(format!("documentSymbol failed: {e}")),
            },
            "diagnostics" => {
                // 诊断由服务器异步推送; 就绪后给一小段产出时间再取快照。
                tokio::time::sleep(Duration::from_millis(300)).await;
                ToolOutput::ok(format_diagnostics(&client.diagnostics_for(&uri)))
            }
            "definition" | "references" | "implementation" | "typeDefinition" | "hover" => {
                let (line, ch) = match position(&args) {
                    Ok(p) => p,
                    Err(e) => return Ok(ToolOutput::error(e)),
                };
                match op {
                    "definition" => locations(client.definition(&uri, line, ch).await, op),
                    "implementation" => locations(client.implementation(&uri, line, ch).await, op),
                    "typeDefinition" => locations(client.type_definition(&uri, line, ch).await, op),
                    "references" => {
                        let incl = args
                            .get("include_declaration")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        locations(client.references(&uri, line, ch, incl).await, op)
                    }
                    _ => match client.hover(&uri, line, ch).await {
                        Ok(v) => ok_or_empty(format_hover(&v), "No hover info at this position (the cursor may not be on a symbol)."),
                        Err(e) => ToolOutput::error(format!("hover failed: {e}")),
                    },
                }
            }
            other => ToolOutput::error(format!("unknown operation: {other}")),
        };
        Ok(result)
    }
}

/// 位置查询结果 → ToolOutput (Err → 工具错误; 空 → 带操作语境的提示, 而非裸 "No results")。
fn locations(res: Result<Value, syncode_lsp::LspError>, op: &str) -> ToolOutput {
    match res {
        Ok(v) => {
            let empty = match op {
                "definition" => "No definition found at this position (the cursor may not be on a symbol).",
                "implementation" => "No implementations found for the symbol at this position.",
                "typeDefinition" => "No type definition found at this position.",
                "references" => "No references found for the symbol at this position.",
                _ => "No results at this position.",
            };
            ok_or_empty(format_locations(&v), empty)
        }
        Err(e) => ToolOutput::error(format!("{op} failed: {e}")),
    }
}

fn ok_or_empty(text: String, empty_msg: &str) -> ToolOutput {
    if text.trim().is_empty() {
        ToolOutput::ok(empty_msg.to_string())
    } else {
        ToolOutput::ok(text)
    }
}

/// 区分「服务器死了」与「还在索引」—— 否则模型会把"没就绪"误读成"符号不存在" (review #15)。
fn not_ready(is_dead: bool) -> ToolOutput {
    if is_dead {
        ToolOutput::error(
            "The language server has stopped (it may have crashed). The next Lsp call will restart \
             it — retry the query.",
        )
    } else {
        ToolOutput::ok(
            "The language server is still indexing the project, so results are not ready yet. \
             Retry the query in a moment.",
        )
    }
}

fn server_unavailable(server: &str, detail: &str) -> ToolOutput {
    ToolOutput::error(format!(
        "could not start the language server '{server}' ({detail}). Is it installed and on PATH?"
    ))
}

/// 没有为该扩展名配置语言服务器时的提示 (写给模型读)。
fn unsupported_ext(path: &Path) -> ToolOutput {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    ToolOutput::error(format!(
        "No language server is configured for '.{ext}' files. Supported: Rust (.rs), Go (.go), \
         Python (.py/.pyi), TypeScript/JavaScript (.ts/.tsx/.js/.jsx), C/C++ (.c/.h/.cpp/.hpp/...)."
    ))
}

fn position(args: &Value) -> Result<(u32, u32), String> {
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .ok_or("line is required for this operation (1-based)")?;
    let ch = args
        .get("character")
        .and_then(Value::as_u64)
        .ok_or("character is required for this operation (1-based)")?;
    // 1-based (编辑器/cat -n) → LSP 0-based。
    Ok((line.saturating_sub(1) as u32, ch.saturating_sub(1) as u32))
}

// 工程根查找 (按语言标志文件) 已移到 `syncode_lsp::lang::workspace_root_for` (多语言, §5.3)。

// ---- LSP JSON 结果 → 写给模型读的文本 ----

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

/// DocumentSymbol[] (层级) 或 SymbolInformation[] (扁平) → 缩进的 `kind name [line:col]`。
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
    let kind = s
        .get("kind")
        .and_then(Value::as_u64)
        .map(symbol_kind_name)
        .unwrap_or("symbol");
    let pos = range
        .and_then(range_start_1based)
        .map(|(l, c)| format!(" [{l}:{c}]"))
        .unwrap_or_default();
    out.push(format!("{}{kind} {name}{pos}", "  ".repeat(depth)));
    if let Some(children) = s.get("children").and_then(Value::as_array) {
        for c in children {
            collect_symbol(c, depth + 1, out);
        }
    }
}

/// workspace/symbol SymbolInformation[] → 每行 `path:line:col  kind name`。
fn format_workspace_symbols(v: &Value) -> String {
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for s in arr {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = s
                .get("kind")
                .and_then(Value::as_u64)
                .map(symbol_kind_name)
                .unwrap_or("symbol");
            if let Some(loc) = s.get("location") {
                let uri = loc.get("uri").and_then(Value::as_str).unwrap_or("");
                let pos = loc.get("range").and_then(range_start_1based);
                if let Some((l, c)) = pos {
                    out.push(format!("{}:{}:{}  {kind} {name}", uri_to_path(uri), l, c));
                    continue;
                }
            }
            out.push(format!("{kind} {name}"));
        }
    }
    out.join("\n")
}

/// Hover.contents (MarkupContent / MarkedString / 其数组) → 纯文本, 末尾附位置 (若有, review #32)。
fn format_hover(v: &Value) -> String {
    let body = match v.get("contents") {
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
    };
    match v.get("range").and_then(range_start_1based) {
        Some((l, c)) if !body.trim().is_empty() => format!("{body}\n(at {l}:{c})"),
        _ => body,
    }
}

/// Diagnostic[] → 每行 `line:col severity: message`。
fn format_diagnostics(diags: &[Value]) -> String {
    if diags.is_empty() {
        return "No diagnostics (the file is clean, or none have been reported yet).".to_string();
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

/// LSP `SymbolKind` 数值 → 名称 (常用项, 其余 "symbol")。
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
