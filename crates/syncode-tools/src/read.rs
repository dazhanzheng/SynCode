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
         - By default the whole file is returned; for a large file pass offset (1-based start \
         line) and limit (number of lines) to read just that window.\n\
         - Output is in `cat -n` format: every line is prefixed with its line number and a tab, so \
         line numbers line up with Grep/AstGrep results.\n\
         - You must Read a file before you can Edit or Write it — those tools error without a prior Read.\n\
         - Re-reading an unchanged file returns a short 'unchanged' notice instead of the full content.\n\
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

/// 取行窗口并加 `cat -n` 行号 (右对齐 6 宽 + tab + 内容)。1-based offset, limit 行;
/// 无 offset/limit 则整文件。行号用**真实文件行号** (offset-aware), 让模型能与 Grep/AstGrep 的
/// `path:line:` 输出对齐定位。注意: 缓存里仍存**裸全文** (供 Edit 精确匹配 + stale 比较), 行号只进展示。
fn apply_window(content: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = (offset.unwrap_or(1).max(1) as usize) - 1;
    let end = match limit {
        Some(l) => (start + l as usize).min(lines.len()),
        None => lines.len(),
    };
    lines
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}
