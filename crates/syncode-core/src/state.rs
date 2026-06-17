//! 对话状态持久化 (D2/D4): full 原文 log = **append-only event log**; checkpoint + 回滚走 seq 截断。
//! 持久化到 SQLite (`rusqlite` bundled, 单文件), **不用真 git** (设计文档 §8: 更轻、可查、in-process)。
//!
//! canonical = 全文 (含完整 reasoning_content / 工具结果); 发送给模型的投影由 `llm::context` 现算 (D1)。
//! 原则: canonical **只前进或截断到 checkpoint**, 不改历史内容 —— 失败尝试丢弃即可, 不会「一直后退」。

use rusqlite::{params, Connection};
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
pub struct SessionStore {
    conn: Connection,
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
             );",
        )?;
        Ok(Self { conn })
    }

    /// 追加一条消息到 canonical log, 返回其 seq (从 0 起, 每 session 独立)。
    pub fn append(&self, session_id: &str, message: &Message) -> Result<i64> {
        let seq = self.next_seq(session_id)?;
        let body = serde_json::to_string(message)?;
        self.conn.execute(
            "INSERT INTO events (session_id, seq, role, body) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, seq, role_str(message), body],
        )?;
        Ok(seq)
    }

    /// 载入 canonical 全文 log (按 seq 升序)。
    pub fn load(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self
            .conn
            .prepare("SELECT body FROM events WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<Message>(&row?)?);
        }
        Ok(out)
    }

    /// 当前事件数。
    pub fn len(&self, session_id: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
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
        let seq = self.max_seq(session_id)?;
        self.conn.execute(
            "INSERT INTO checkpoints (session_id, label, seq) VALUES (?1, ?2, ?3)",
            params![session_id, label, seq],
        )?;
        Ok(seq)
    }

    /// 回滚: 删除 `seq > up_to_seq` 的事件 (截断到某 checkpoint)。后续 append 从截断点续号。
    pub fn rollback(&self, session_id: &str, up_to_seq: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM events WHERE session_id = ?1 AND seq > ?2",
            params![session_id, up_to_seq],
        )?;
        Ok(())
    }

    fn max_seq(&self, session_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(seq), -1) FROM events WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    fn next_seq(&self, session_id: &str) -> Result<i64> {
        Ok(self.max_seq(session_id)? + 1)
    }
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
}
