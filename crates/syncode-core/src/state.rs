//! 对话状态持久化 (D2/D4): full 原文 log = **append-only event log**; checkpoint + 回滚走 seq 截断。
//! 持久化到 SQLite (`rusqlite` bundled, 单文件), **不用真 git** (设计文档 §8: 更轻、可查、in-process)。
//!
//! canonical = 全文 (含完整 reasoning_content / 工具结果); 发送给模型的投影由 `llm::context` 现算 (D1)。
//! 原则: canonical **只前进或截断到 checkpoint**, 不改历史内容 —— 失败尝试丢弃即可, 不会「一直后退」。

use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;
use syncode_llm::wire::Message;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StateError>;

/// 一个 session 的 append-only 事件存储 + checkpoint。
///
/// `Connection` 用 `Mutex` 包裹 —— 使 `SessionStore` (及持有它的 `AgentLoop`) 成为 `Send + Sync`,
/// 从而 agent 的 future 可在多线程 runtime 上安全使用 (子 agent 派生需要 loop 的 future 为 `Send`)。
pub struct SessionStore {
    conn: Mutex<Connection>,
}

impl SessionStore {
    /// 打开 (或新建) 一个磁盘库。
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// 内存库 (测试 / 易失场景)。
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 session_id TEXT NOT NULL,
                 seq        INTEGER NOT NULL,
                 role       TEXT NOT NULL,
                 body       TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq)
             );
             CREATE TABLE IF NOT EXISTS checkpoints (
                 session_id TEXT NOT NULL,
                 label      TEXT NOT NULL,
                 seq        INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
                 session_id TEXT PRIMARY KEY,
                 workspace  TEXT NOT NULL,
                 title      TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// 追加一条消息到 canonical log, 返回其 seq (从 0 起, 每 session 独立)。
    pub fn append(&self, session_id: &str, message: &Message) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let seq = max_seq(&conn, session_id)? + 1;
        let body = serde_json::to_string(message)?;
        conn.execute(
            "INSERT INTO events (session_id, seq, role, body) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, seq, role_str(message), body],
        )?;
        Ok(seq)
    }

    /// 载入 canonical 全文 log (按 seq 升序)。
    pub fn load(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT body FROM events WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<Message>(&row?)?);
        }
        Ok(out)
    }

    /// 当前事件数。
    pub fn len(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn is_empty(&self, session_id: &str) -> Result<bool> {
        Ok(self.len(session_id)? == 0)
    }

    /// 打一个 checkpoint (记录当前最大 seq), 返回该 seq (空 log 为 -1)。
    pub fn checkpoint(&self, session_id: &str, label: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let seq = max_seq(&conn, session_id)?;
        conn.execute(
            "INSERT INTO checkpoints (session_id, label, seq) VALUES (?1, ?2, ?3)",
            params![session_id, label, seq],
        )?;
        Ok(seq)
    }

    /// 回滚: 删除 `seq > up_to_seq` 的事件 (截断到某 checkpoint)。后续 append 从截断点续号。
    pub fn rollback(&self, session_id: &str, up_to_seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM events WHERE session_id = ?1 AND seq > ?2",
            params![session_id, up_to_seq],
        )?;
        Ok(())
    }

    /// 登记一条会话元数据 (幂等: 已存在则不动)。
    pub fn ensure_session(&self, session_id: &str, workspace: &str, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (session_id, workspace, title, created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?3)",
            params![session_id, workspace, now_ms],
        )?;
        Ok(())
    }

    /// 更新会话的活跃时间 (列表按它降序排)。
    pub fn touch_session(&self, session_id: &str, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params![session_id, now_ms],
        )?;
        Ok(())
    }

    /// 若 title 仍空, 用 (首条 user 消息压成的) 标题填上; 已有则不动。
    pub fn set_title_if_absent(&self, session_id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET title = ?2 WHERE session_id = ?1 AND (title IS NULL OR title = '')",
            params![session_id, title],
        )?;
        Ok(())
    }

    /// 列出某 workspace 的会话, 最近活跃在前。
    pub fn list_sessions(&self, workspace: &str) -> Result<Vec<SessionMeta>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace, title, created_at, updated_at
             FROM sessions WHERE workspace = ?1 ORDER BY updated_at DESC, created_at DESC",
        )?;
        let rows = stmt.query_map(params![workspace], |r| {
            Ok(SessionMeta {
                session_id: r.get(0)?,
                workspace: r.get(1)?,
                title: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// 旧库迁移: 给 events 里出现过、但 sessions 表还没登记的 session_id 各补一行元数据。
    /// workspace 暂记为 session_id 本身 (旧方案 session_id 即 workspace 路径); title 取首条 user 消息。
    pub fn backfill_sessions(&self, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT session_id FROM events
                 WHERE session_id NOT IN (SELECT session_id FROM sessions)",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in ids {
            let title = first_user_title(&conn, &id)?;
            conn.execute(
                "INSERT OR IGNORE INTO sessions (session_id, workspace, title, created_at, updated_at)
                 VALUES (?1, ?1, ?2, ?3, ?3)",
                params![id, title, now_ms],
            )?;
        }
        Ok(())
    }
}

/// 某 session 当前最大 seq (空 log 为 -1)。取已锁的 `&Connection` 以避免可重入死锁。
fn max_seq(conn: &Connection, session_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), -1) FROM events WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?)
}

fn role_str(m: &Message) -> &'static str {
    use syncode_llm::wire::Role;
    match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// 一条会话的元数据 (供「浏览历史会话」列表)。
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub session_id: String,
    pub workspace: String,
    /// 列表标题 (首条 user 消息压成的一行); 可能为空。
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 把一条消息内容压成一行短标题 (≤60 字符)。
pub fn title_snippet(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = one_line.chars().count();
    let head: String = one_line.chars().take(60).collect();
    if n > 60 {
        format!("{head}…")
    } else {
        head
    }
}

/// 某 session 首条 user 消息压成的标题 (无则 None)。
fn first_user_title(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM events WHERE session_id = ?1 AND role = 'user' ORDER BY seq LIMIT 1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(body
        .and_then(|b| serde_json::from_str::<Message>(&b).ok())
        .and_then(|m| m.content)
        .map(|c| title_snippet(&c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_load_roundtrips_in_order() {
        let s = SessionStore::in_memory().unwrap();
        s.append("sess", &Message::system("sys")).unwrap();
        s.append("sess", &Message::user("hi")).unwrap();
        let log = s.load("sess").unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].content.as_deref(), Some("sys"));
        assert_eq!(log[1].content.as_deref(), Some("hi"));
    }

    #[test]
    fn seq_monotonic_and_isolated_per_session() {
        let s = SessionStore::in_memory().unwrap();
        assert_eq!(s.append("a", &Message::user("a0")).unwrap(), 0);
        assert_eq!(s.append("a", &Message::user("a1")).unwrap(), 1);
        assert_eq!(s.append("b", &Message::user("b0")).unwrap(), 0); // 另一 session 独立计数
        assert_eq!(s.len("a").unwrap(), 2);
        assert_eq!(s.len("b").unwrap(), 1);
    }

    #[test]
    fn checkpoint_then_rollback_truncates() {
        let s = SessionStore::in_memory().unwrap();
        s.append("sess", &Message::user("u1")).unwrap();
        let cp = s.checkpoint("sess", "after-u1").unwrap(); // seq 0
        s.append("sess", &Message::user("u2")).unwrap();
        s.append("sess", &Message::user("u3")).unwrap();
        assert_eq!(s.len("sess").unwrap(), 3);
        s.rollback("sess", cp).unwrap(); // 回到 checkpoint: 删 seq>0
        let log = s.load("sess").unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].content.as_deref(), Some("u1"));
    }

    #[test]
    fn append_after_rollback_continues_from_truncated_seq() {
        // 回滚后再 append: seq 从截断点续 (canonical 只前进/截断, 不跳号、不「一直后退」)。
        let s = SessionStore::in_memory().unwrap();
        s.append("sess", &Message::user("u1")).unwrap();
        s.append("sess", &Message::user("u2")).unwrap();
        s.rollback("sess", 0).unwrap();
        assert_eq!(s.append("sess", &Message::user("u2b")).unwrap(), 1);
        assert_eq!(s.len("sess").unwrap(), 2);
    }

    #[test]
    fn reasoning_content_survives_roundtrip_full() {
        // canonical 存全文: reasoning_content (含 Some(""), None 区分) 完整往返。
        let s = SessionStore::in_memory().unwrap();
        let mut m = Message::user("x");
        m.reasoning_content = Some("full chain of thought".to_string());
        s.append("sess", &m).unwrap();
        let log = s.load("sess").unwrap();
        assert_eq!(log[0].reasoning_content.as_deref(), Some("full chain of thought"));
    }

    #[test]
    fn backfill_creates_session_meta_with_title_from_first_user_msg() {
        let s = SessionStore::in_memory().unwrap();
        // 旧库: 直接以「路径作 session_id」append, 没有 sessions 行。
        s.append("/ws/proj", &Message::system("sys")).unwrap();
        s.append("/ws/proj", &Message::user("fix the parser bug")).unwrap();
        s.backfill_sessions(1000).unwrap();
        let list = s.list_sessions("/ws/proj").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "/ws/proj");
        assert_eq!(list[0].workspace, "/ws/proj");
        assert_eq!(list[0].title.as_deref(), Some("fix the parser bug"));
        // 再次 backfill 不重复登记。
        s.backfill_sessions(2000).unwrap();
        assert_eq!(s.list_sessions("/ws/proj").unwrap().len(), 1);
    }

    #[test]
    fn list_sessions_scopes_to_workspace_and_orders_by_recency() {
        let s = SessionStore::in_memory().unwrap();
        s.ensure_session("a", "/ws", 100).unwrap();
        s.ensure_session("b", "/ws", 200).unwrap();
        s.ensure_session("other", "/ws2", 300).unwrap();
        s.touch_session("a", 500).unwrap(); // a 变成最近活跃
        let ws: Vec<String> =
            s.list_sessions("/ws").unwrap().into_iter().map(|m| m.session_id).collect();
        assert_eq!(ws, vec!["a", "b"]);
        assert_eq!(s.list_sessions("/ws2").unwrap().len(), 1);
    }

    #[test]
    fn ensure_idempotent_and_title_set_once() {
        let s = SessionStore::in_memory().unwrap();
        s.ensure_session("x", "/ws", 1).unwrap();
        s.ensure_session("x", "/ws", 999).unwrap(); // no-op, created_at 不变
        s.set_title_if_absent("x", "first").unwrap();
        s.set_title_if_absent("x", "second").unwrap(); // 已有 → 忽略
        let list = s.list_sessions("/ws").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title.as_deref(), Some("first"));
        assert_eq!(list[0].created_at, 1);
    }
}
