//! 共享文件状态缓存 (借鉴 CC `readFileState` / `fileStateCache`, §10)。
//!
//! 文件工具间「联动」的底座: **Read 写入、Edit/Write 读取**并据此做「必先读」+ stale 检测,
//! 防止模型基于过期/幻觉的快照覆盖磁盘上的实时改动 (用户 / linter / 并发)。Grep/Glob 不碰本缓存。
//!
//! 当前: `Mutex<HashMap>`, 无 LRU/容量上限 (= 未来 TODO; CC 是 LRU 100 条 / 25MB)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 一个文件被 Read/Write/Edit 时记录的状态。
#[derive(Debug, Clone)]
pub struct FileState {
    /// 文件内容 (LF 归一化)。stale 检测的「内容兜底比较」用。
    pub content: String,
    /// 读/写时刻的 mtime (floor 到毫秒)。stale 检测主依据。
    pub timestamp: i64,
    /// Read 用的行窗口起点 (1-based); 写产生的条目为 `None`。
    pub offset: Option<u64>,
    /// Read 用的行数上限; 写产生的条目为 `None`。
    pub limit: Option<u64>,
    /// 是否为自动注入的「部分视图」(如截断的 CLAUDE.md) —— 强制模型真 Read 一次。
    pub is_partial_view: bool,
}

/// session 级共享文件状态缓存。内部可变 (`Mutex`), 经 `Arc` 跨工具/任务共享。
#[derive(Debug, Default)]
pub struct FileStateCache {
    inner: Mutex<HashMap<PathBuf, FileState>>,
}

impl FileStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path) -> Option<FileState> {
        self.inner.lock().unwrap().get(&normalize(path)).cloned()
    }

    pub fn set(&self, path: &Path, state: FileState) {
        self.inner.lock().unwrap().insert(normalize(path), state);
    }

    pub fn has(&self, path: &Path) -> bool {
        self.inner.lock().unwrap().contains_key(&normalize(path))
    }
}

/// 规范化为绝对路径 key: `std::path::absolute` + 在 Windows 折叠 `\`→`/`, 使 Read 与随后的
/// Edit (即便路径写法不同) 命中同一条目。(lexical `..` 解析与 symlink canonicalize = 未来 TODO。)
fn normalize(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(abs.to_string_lossy().replace('\\', "/"))
}
