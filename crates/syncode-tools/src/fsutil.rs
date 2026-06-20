//! 文件工具共享底层: LF 归一读、mtime、原子写 (§5 第一梯队)。

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::UNIX_EPOCH;
use syncode_core::file_state::FileStateCache;
use syncode_core::tool::FileDiff;

/// 算一次改动的 unified diff (供 UI diff 视图)。`old`/`new` 应是 LF 归一文本。无变化 → `None`。
/// context_radius=3 (标准 unified 上下文行数)。新文件传 `old = ""`。
pub fn make_diff(path: &str, old: &str, new: &str) -> Option<FileDiff> {
    if old == new {
        return None;
    }
    let unified = similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string();
    if unified.trim().is_empty() {
        return None;
    }
    Some(FileDiff { path: path.to_string(), unified })
}

/// 「必先读」+ stale 检测 (Write/Edit 对**已存在文件**用)。返回 `Some(写给模型读的错误串)` 表示拒绝,
/// `None` 表示放行。逐字文案照搬 CC (§10, 写给模型自纠)。
pub fn check_read_and_fresh(files: &FileStateCache, path: &Path) -> Option<&'static str> {
    let cached = match files.get(path) {
        Some(s) if !s.is_partial_view => s,
        // 无缓存 或 仅「部分视图」→ 必须先真正 Read 一次。
        _ => return Some("File has not been read yet. Read it first before writing to it."),
    };
    if let Ok(cur) = mtime_ms(path) {
        if cur > cached.timestamp {
            // mtime 变新 → 全文读时用内容相等兜底 (绕 Windows mtime 抖动); 否则判为 stale。
            let same = cached.offset.is_none()
                && cached.limit.is_none()
                && read_text_lf(path).map(|(c, _)| c == cached.content).unwrap_or(false);
            if !same {
                return Some("File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.");
            }
        }
    }
    None
}

/// 读文件为文本: UTF-16LE BOM 转码, 否则按 UTF-8; 一律 CRLF→LF 归一。返回 `(content, mtime_ms)`。
pub fn read_text_lf(path: &Path) -> std::io::Result<(String, i64)> {
    let bytes = fs::read(path)?;
    let content = decode_text(&bytes).replace("\r\n", "\n");
    Ok((content, mtime_ms(path)?))
}

fn decode_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        // UTF-16LE BOM
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 文件原本是否用 CRLF 换行 (任一处 `\r\n` 即判 CRLF)。用于 Edit 写回时保留原换行风格,
/// 避免在 Windows 上把整文件的 CRLF 改成 LF (CC 行为: Edit 保留原 EOL)。
pub fn file_is_crlf(path: &Path) -> bool {
    fs::read(path)
        .map(|b| b.windows(2).any(|w| w == b"\r\n"))
        .unwrap_or(false)
}

/// 文件 mtime, floor 到毫秒 (stale 检测主依据, 对照 CC `floor(mtimeMs)`)。
pub fn mtime_ms(path: &Path) -> std::io::Result<i64> {
    let mt = fs::metadata(path)?.modified()?;
    Ok(mt.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64)
}

/// 原子写: 同目录临时文件 + fsync + rename (写一半崩了也不会留半截文件)。
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
