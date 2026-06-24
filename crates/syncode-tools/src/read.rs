//! Read: `offset`/`limit` 行窗口 (借鉴 CC, §10)。成功后把全文写入共享 `FileStateCache`,
//! 供 Edit/Write 做「必先读」+ stale 检测。

use crate::fsutil;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use syncode_core::file_state::FileState;
use syncode_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};

/// 重复读未变文件时返回的存根 (逐字照搬 CC, 省 token)。
const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current — refer to that instead of re-reading.";

/// Read 默认行窗口上限 (无 `limit` 时): 防一个 Read 把整个大文件灌爆 context (评估 P1「无界输入」)。
const DEFAULT_MAX_LINES: usize = 2000;
/// 单行字符上限: 防 minified / 巨长单行撑爆 context。
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the local filesystem and return its text content.\n\
         Usage:\n\
         - file_path must be an absolute path, not a relative one.\n\
         - By default up to the first 2000 lines are returned; pass offset (1-based start \
         line) and limit (number of lines) to read just that window.\n\
         - Output is in `cat -n` format: every line is prefixed with its line number and a tab, so \
         line numbers line up with Grep/AstGrep results.\n\
         - You must Read a file before you can Edit or Write it — those tools error without a prior Read.\n\
         - Re-reading an unchanged file returns a short 'unchanged' notice instead of the full content.\n\
         - Very long lines are truncated; binary files return a short stub instead of raw bytes.\n\
         - This reads files, not directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file to read." },
                "offset": { "type": "integer", "description": "1-based line to start reading from." },
                "limit": { "type": "integer", "description": "Maximum number of lines to read." }
            },
            "required": ["file_path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("file_path is required".into()))?;
        let path = PathBuf::from(file_path);
        let offset = args.get("offset").and_then(Value::as_u64);
        let limit = args.get("limit").and_then(Value::as_u64);

        // dedup (省 token): 同窗口 + mtime 未变 → 返回存根, 不重发全文。
        if let Some(cached) = ctx.files.get(&path) {
            if !cached.is_partial_view
                && cached.offset == offset
                && cached.limit == limit
                && fsutil::mtime_ms(&path).map(|m| m == cached.timestamp).unwrap_or(false)
            {
                return Ok(ToolOutput::ok(FILE_UNCHANGED_STUB));
            }
        }

        if path.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{file_path} is a directory, not a file"
            )));
        }
        let (content, mtime) = fsutil::read_text_lf(&path)
            .map_err(|e| ToolError::Exec(format!("could not read {file_path}: {e}")))?;

        // 二进制嗅探: 解码后前若干 KB 出现 NUL → 不把 lossy 文本灌进 context, 回简短存根。
        // 缓存里记一个 partial-view 标记 (无可编辑全文), 让后续 Edit/Write fail-closed 拒绝。
        if fsutil::looks_binary(&content) {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            ctx.files.set(
                &path,
                FileState { content: String::new(), timestamp: mtime, offset, limit, is_partial_view: true },
            );
            return Ok(ToolOutput::ok(format!(
                "[binary file, {bytes} bytes — not shown. This tool returns text; use a dedicated tool to inspect binary content.]"
            )));
        }

        let view = apply_window(&content, offset, limit);

        // 写缓存: 永远存**全文** content (stale 内容兜底比较用), 记录本次窗口 offset/limit。
        ctx.files.set(
            &path,
            FileState {
                content,
                timestamp: mtime,
                offset,
                limit,
                is_partial_view: false,
            },
        );
        Ok(ToolOutput::ok(view))
    }
}

/// 取行窗口并加 `cat -n` 行号 (右对齐 6 宽 + tab + 内容)。1-based offset; 行号用**真实文件行号**
/// (offset-aware), 让模型能与 Grep/AstGrep 的 `path:line:` 输出对齐定位。
///
/// 上限 (评估 P1「无界输入」): 无 `limit` 时最多 `DEFAULT_MAX_LINES` 行, 单行最多 `MAX_LINE_CHARS`
/// 字符, 截断处都给模型可操作的提示 (怎么继续 / 被截多少)。注意: 缓存里仍存**裸全文** (供 Edit
/// 精确匹配 + stale 比较), 这些上限只作用于**返回给模型的视图**。
fn apply_window(content: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = (offset.unwrap_or(1).max(1) as usize) - 1;

    // offset 越界 → 给可操作提示, 而非空串 (空串与「文件为空」无法区分)。
    if start >= total {
        if total == 0 {
            return String::new();
        }
        return format!("[offset {} is past end of file ({total} lines); valid range is 1..={total}]", start + 1);
    }

    // 无 limit 时套默认窗口上限; 有 limit 则尊重模型给的窗口。
    let window = limit.map(|l| l as usize).unwrap_or(DEFAULT_MAX_LINES);
    let end = start.saturating_add(window).min(total);

    let mut out = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, truncate_line(line)))
        .collect::<Vec<_>>()
        .join("\n");

    if end < total {
        out.push_str(&format!(
            "\n\n[truncated: showing lines {}-{end} of {total}; pass offset={} to continue]",
            start + 1,
            end + 1
        ));
    }
    out
}

/// 单行截断: 超 `MAX_LINE_CHARS` 字符的行 (minified JS / data-URI 等) 截断 + 标注总长,
/// 避免单行就撑爆 context。按 char 计 (不按字节), 不切坏多字节。
fn truncate_line(line: &str) -> String {
    let n = line.chars().count();
    if n > MAX_LINE_CHARS {
        let head: String = line.chars().take(MAX_LINE_CHARS).collect();
        format!("{head} … [line truncated; {n} chars total]")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limit_caps_at_default_and_marks_truncation() {
        let content = (1..=2500).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let out = apply_window(&content, None, None);
        assert!(out.starts_with("     1\tline1\n"));
        assert!(out.contains("  2000\tline2000"));
        assert!(!out.contains("line2001"));
        assert!(out.contains("[truncated: showing lines 1-2000 of 2500; pass offset=2001 to continue]"));
    }

    #[test]
    fn explicit_window_returns_slice_with_continue_hint_when_more_follows() {
        let content = (1..=100).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let out = apply_window(&content, Some(10), Some(3));
        assert!(out.starts_with("    10\tL10\n    11\tL11\n    12\tL12"), "{out}");
        assert!(out.contains("[truncated: showing lines 10-12 of 100; pass offset=13 to continue]"), "{out}");
    }

    #[test]
    fn explicit_window_reaching_eof_has_no_marker() {
        let content = (1..=5).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let out = apply_window(&content, Some(3), Some(10)); // lines 3..=5 then EOF
        assert_eq!(out, "     3\tL3\n     4\tL4\n     5\tL5");
    }

    #[test]
    fn small_file_is_returned_whole_without_marker() {
        let out = apply_window("a\nb\nc", None, None);
        assert_eq!(out, "     1\ta\n     2\tb\n     3\tc");
    }

    #[test]
    fn offset_past_eof_explains_instead_of_empty() {
        let out = apply_window("a\nb", Some(10), None);
        assert!(out.contains("past end of file"), "{out}");
        assert!(out.contains("1..=2"), "{out}");
    }

    #[test]
    fn long_line_is_truncated_with_count() {
        let out = apply_window(&"x".repeat(5000), None, None);
        assert!(out.contains("[line truncated; 5000 chars total]"), "{out}");
        assert!(out.chars().count() < 2300, "truncated view should be bounded");
    }
}
