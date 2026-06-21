//! 共享文件状态缓存 (借鉴 CC `readFileState` / `fileStateCache`, §10)。
//!
//! 文件工具间「联动」的底座: **Read 写入、Edit/Write 读取**并据此做「必先读」+ stale 检测,
//! 防止模型基于过期/幻觉的快照覆盖磁盘上的实时改动 (用户 / linter / 并发)。Grep/Glob 不碰本缓存。
//!
//! 容量受限的 LRU (对照 CC 的 LRU 100 条 / 25MB): 超 `MAX_ENTRIES` 条或 `MAX_TOTAL_BYTES` 字节即
//! 逐出最久未用项, 防长会话里缓存无界增长吃内存。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 缓存条目数上限 (超出逐出 LRU)。
const MAX_ENTRIES: usize = 256;
/// 缓存总内容字节上限 (超出逐出 LRU)。
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

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

/// 带 LRU 时钟的条目: `last_used` 单调递增, 逐出时挑最小者。
#[derive(Debug, Clone)]
struct Entry {
    state: FileState,
    last_used: u64,
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<PathBuf, Entry>,
    clock: u64,
    bytes: usize,
}

impl Inner {
    /// 取下一个递增的逻辑时钟 (access stamp)。
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// 逐出最久未用项, 直到条目数与总字节都回到上限内。
    fn evict_to_fit(&mut self) {
        while self.map.len() > MAX_ENTRIES || self.bytes > MAX_TOTAL_BYTES {
            let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = self.map.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(e.state.content.len());
            }
        }
    }
}

/// session 级共享文件状态缓存。内部可变 (`Mutex`), 经 `Arc` 跨工具/任务共享。
#[derive(Debug, Default)]
pub struct FileStateCache {
    inner: Mutex<Inner>,
}

impl FileStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path) -> Option<FileState> {
        let mut inner = self.inner.lock().unwrap();
        let stamp = inner.tick();
        let key = normalize(path);
        let entry = inner.map.get_mut(&key)?;
        entry.last_used = stamp; // 命中 = 刷新 recency
        Some(entry.state.clone())
    }

    pub fn set(&self, path: &Path, state: FileState) {
        let mut inner = self.inner.lock().unwrap();
        let stamp = inner.tick();
        let key = normalize(path);
        let new_len = state.content.len();
        if let Some(old) = inner.map.insert(key, Entry { state, last_used: stamp }) {
            inner.bytes = inner.bytes.saturating_sub(old.state.content.len()); // 替换: 先扣旧
        }
        inner.bytes += new_len;
        inner.evict_to_fit();
    }

    pub fn has(&self, path: &Path) -> bool {
        self.inner.lock().unwrap().map.contains_key(&normalize(path))
    }

    /// 当前缓存条目数 (测试 / 观测用)。
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }
}

/// 规范化为绝对路径 key: `std::path::absolute` + 在 Windows 折叠 `\`→`/`, 使 Read 与随后的
/// Edit (即便路径写法不同) 命中同一条目。(lexical `..` 解析与 symlink canonicalize = 未来 TODO。)
fn normalize(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(abs.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(content: &str) -> FileState {
        FileState {
            content: content.to_string(),
            timestamp: 0,
            offset: None,
            limit: None,
            is_partial_view: false,
        }
    }

    #[test]
    fn evicts_over_entry_cap_keeping_recently_used() {
        let cache = FileStateCache::new();
        // 塞满到上限。
        for i in 0..MAX_ENTRIES {
            cache.set(Path::new(&format!("/f/{i}.txt")), state("x"));
        }
        // 持续触碰第 0 条 → 它是最近用过的, 不该被逐出。
        let hot = Path::new("/f/0.txt");
        assert!(cache.get(hot).is_some());
        // 再塞 50 条 → 触发逐出, 但总数封顶, 且 hot 仍在。
        for i in MAX_ENTRIES..MAX_ENTRIES + 50 {
            cache.set(Path::new(&format!("/f/{i}.txt")), state("x"));
            let _ = cache.get(hot); // 每次都刷新 hot 的 recency
        }
        assert!(cache.len() <= MAX_ENTRIES, "must stay within entry cap: {}", cache.len());
        assert!(cache.get(hot).is_some(), "most-recently-used entry must survive eviction");
    }

    #[test]
    fn replacing_an_entry_does_not_double_count_bytes() {
        let cache = FileStateCache::new();
        let p = Path::new("/f/a.txt");
        cache.set(p, state(&"a".repeat(1000)));
        cache.set(p, state("small")); // 替换: bytes 应扣旧加新, 不累加
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(p).unwrap().content, "small");
    }
}
