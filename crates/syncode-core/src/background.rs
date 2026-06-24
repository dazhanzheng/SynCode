//! 后台任务注册表 (路线图 §5.5): 让 Bash 把长命令 (build/test/dev-server) 丢后台跑, 之后增量查输出 / 杀。
//!
//! 设计: 一个 [`BackgroundTask`] 持有**累积输出缓冲** + **状态** + **杀进程闭包** (type-erased, 故 core 不依赖
//! sandbox/tokio 的进程类型)。Bash 起进程后建 task、注册、再 `tokio::spawn` 一个抽水任务把 stdout/stderr 增量
//! 灌进缓冲并在退出时落状态。`BashOutput` 工具按 id 读「上次之后的新输出」+ 状态, 或杀任务。
//! 放进 `ToolCtx` 跨工具调用共享 (与文件缓存 / LSP 同款持久活状态)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::Arc;

/// 注册表上限: 超过则回收已终止的旧任务 (运行中的不计入回收)。够覆盖任何真实并发后台数。
const MAX_TASKS: usize = 64;

/// 后台任务状态。
#[derive(Debug, Clone)]
pub enum TaskState {
    Running,
    Exited(Option<i32>),
    TimedOut,
    Killed,
}

impl TaskState {
    pub fn is_running(&self) -> bool {
        matches!(self, TaskState::Running)
    }
    /// 写给模型读的一行状态。
    pub fn label(&self) -> String {
        match self {
            TaskState::Running => "running".to_string(),
            TaskState::Exited(Some(c)) => format!("exited (code {c})"),
            TaskState::Exited(None) => "exited (no code / signaled)".to_string(),
            TaskState::TimedOut => "timed out (killed)".to_string(),
            TaskState::Killed => "killed".to_string(),
        }
    }
}

/// 一个后台任务的共享状态 (drain 任务写 output/state, `BashOutput` 工具读)。
pub struct BackgroundTask {
    /// 原始命令 (展示用)。
    pub command: String,
    /// 累积输出 (stdout+stderr 交织; 后台场景可接受交织)。
    pub output: Mutex<String>,
    /// 已被 `read_new` 读到的位置 (增量读游标)。
    pub cursor: Mutex<usize>,
    /// 当前状态。
    pub state: Mutex<TaskState>,
    /// 杀整棵进程树的闭包 (type-erased; Bash 注入 = 调容器的 kill)。
    pub kill: Box<dyn Fn() + Send + Sync>,
}

impl BackgroundTask {
    pub fn new(command: impl Into<String>, kill: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            command: command.into(),
            output: Mutex::new(String::new()),
            cursor: Mutex::new(0),
            state: Mutex::new(TaskState::Running),
            kill,
        })
    }
    pub fn set_state(&self, s: TaskState) {
        *self.state.lock().unwrap() = s;
    }
    /// 终态**一次性闩**: 只有当前还在 Running 才落终态, 已终止则不覆盖 (review fix #9 —— 杀/退竞态下
    /// 别用 Exited 盖掉 Killed)。读 + 写在同一把锁内, 无 check-then-act 缝隙。
    pub fn set_terminal(&self, s: TaskState) {
        let mut g = self.state.lock().unwrap();
        if g.is_running() {
            *g = s;
        }
    }
    /// 增量追加输出, 维持**滑窗**上限 (review fix #11): 超 `max` 时优先丢**已读**前缀并 rebase 游标,
    /// 保证消费者始终看得到最新输出 (而非旧版「到顶就丢最新、永远卡在前 200KB」)。
    pub fn append(&self, chunk: &str, max: usize) {
        let mut o = self.output.lock().unwrap();
        o.push_str(chunk);
        if o.len() <= max {
            return;
        }
        let mut cur = self.cursor.lock().unwrap();
        // 先丢已读前缀 [0..cursor)。
        let overflow = o.len() - max;
        let drop_n = floor_char_boundary(&o, overflow.min(*cur));
        if drop_n > 0 {
            o.replace_range(0..drop_n, "");
            *cur -= drop_n;
        }
        // 若未读数据本身仍超 cap (极少见), 再丢最前未读, 游标归零。
        if o.len() > max {
            let extra = floor_char_boundary(&o, o.len() - max);
            o.replace_range(0..extra, "");
            *cur = cur.saturating_sub(extra);
        }
    }
}

/// 不超过 `idx` 的最近 char 边界 (避免 replace_range 切碎多字节字符)。
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 跨工具共享的后台任务注册表。
#[derive(Default)]
pub struct BackgroundRegistry {
    tasks: Mutex<HashMap<String, Arc<BackgroundTask>>>,
    counter: AtomicU64,
}

impl BackgroundRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 task, 返回稳定 id (如 `bash_1`)。注册时**回收已终止的旧任务** (review fix #8/#10):
    /// 否则 HashMap 只增不减, 每个 Arc<BackgroundTask> 钉住其 Arc<ProcessContainer> (Windows 上 Job
    /// HANDLE 永不 CloseHandle) + 最多 200KB 输出缓冲, 长会话无界泄漏。运行中的任务不动。
    pub fn register(&self, task: Arc<BackgroundTask>) -> String {
        let id = format!("bash_{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let mut map = self.tasks.lock().unwrap();
        map.insert(id.clone(), task);
        if map.len() > MAX_TASKS {
            let terminal: Vec<String> = map
                .iter()
                .filter(|(_, t)| !t.state.lock().unwrap().is_running())
                .map(|(k, _)| k.clone())
                .collect();
            for k in terminal {
                if map.len() <= MAX_TASKS {
                    break;
                }
                map.remove(&k); // 丢最后一个强引用 → 容器 Drop → CloseHandle, 缓冲回收
            }
        }
        id
    }

    pub fn get(&self, id: &str) -> Option<Arc<BackgroundTask>> {
        self.tasks.lock().unwrap().get(id).cloned()
    }

    /// 读「上次之后的新输出」+ 当前状态, 推进游标。任务不存在 → `None`。
    pub fn read_new(&self, id: &str) -> Option<(String, TaskState)> {
        let t = self.get(id)?;
        let out = t.output.lock().unwrap();
        let mut cur = t.cursor.lock().unwrap();
        let start = (*cur).min(out.len());
        let new = out[start..].to_string();
        *cur = out.len();
        Some((new, t.state.lock().unwrap().clone()))
    }

    /// 杀掉一个后台任务 (整树)。任务不存在 → `false`。
    /// **只对仍在运行的任务触发杀进程闭包** (review fix #17): 已终止的任务其 pgid 可能已被 OS 复用,
    /// 再 `killpg` 会误杀无关进程组 —— 故已终止则直接报成功、不再开杀。杀后**无条件**落 Killed (显式杀,
    /// 即便与自然退出竞态也以 Killed 为准)。
    pub fn kill(&self, id: &str) -> bool {
        match self.get(id) {
            Some(t) => {
                let running = t.state.lock().unwrap().is_running();
                if running {
                    (t.kill)();
                    *t.state.lock().unwrap() = TaskState::Killed;
                }
                true
            }
            None => false,
        }
    }

    /// 所有任务的 (id, 命令, 状态)。
    pub fn list(&self) -> Vec<(String, String, TaskState)> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .map(|(id, t)| (id.clone(), t.command.clone(), t.state.lock().unwrap().clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_read_incremental_and_kill() {
        let reg = BackgroundRegistry::new();
        let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let k = killed.clone();
        let task = BackgroundTask::new("sleep 100", Box::new(move || {
            k.store(true, Ordering::SeqCst);
        }));
        let id = reg.register(task.clone());
        assert!(id.starts_with("bash_"));

        // 增量读: 先空, append 后读到新的, 再读为空。
        let (s0, st0) = reg.read_new(&id).unwrap();
        assert_eq!(s0, "");
        assert!(st0.is_running());
        task.append("hello ", 1000);
        task.append("world", 1000);
        let (s1, _) = reg.read_new(&id).unwrap();
        assert_eq!(s1, "hello world");
        let (s2, _) = reg.read_new(&id).unwrap();
        assert_eq!(s2, "", "second read sees no new output");

        // 杀: 触发闭包 + 状态变 Killed。
        assert!(reg.kill(&id));
        assert!(killed.load(Ordering::SeqCst));
        let (_, st) = reg.read_new(&id).unwrap();
        assert!(matches!(st, TaskState::Killed));

        // 未知 id。
        assert!(reg.read_new("bash_999").is_none());
        assert!(!reg.kill("bash_999"));
    }

    #[test]
    fn output_sliding_window_keeps_latest() {
        let reg = BackgroundRegistry::new();
        let task = BackgroundTask::new("x", Box::new(|| {}));
        let id = reg.register(task.clone());
        task.append("AAAA", 10);
        let (s1, _) = reg.read_new(&id).unwrap();
        assert_eq!(s1, "AAAA"); // 游标推进到 4
        // 追加超 cap: 丢已读的 "AAAA", 保留最新的 B 串 (而非卡死在旧数据)。
        task.append("BBBBBBBBBB", 10);
        let (s2, _) = reg.read_new(&id).unwrap();
        assert_eq!(s2, "BBBBBBBBBB", "consumer must see the latest output, not be frozen");
    }

    #[test]
    fn kill_state_not_clobbered_by_later_exit() {
        let reg = BackgroundRegistry::new();
        let task = BackgroundTask::new("x", Box::new(|| {}));
        let id = reg.register(task.clone());
        assert!(reg.kill(&id)); // running → Killed
        // waiter 晚到: set_terminal(Exited) 必须不覆盖 Killed。
        task.set_terminal(TaskState::Exited(Some(0)));
        let (_, st) = reg.read_new(&id).unwrap();
        assert!(matches!(st, TaskState::Killed), "explicit kill must stay Killed");
    }

    #[test]
    fn kill_on_terminal_task_does_not_refire() {
        use std::sync::atomic::AtomicUsize;
        let reg = BackgroundRegistry::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        let task = BackgroundTask::new("x", Box::new(move || {
            f.fetch_add(1, Ordering::SeqCst);
        }));
        let id = reg.register(task.clone());
        task.set_terminal(TaskState::Exited(Some(0))); // 已自然退出
        assert!(reg.kill(&id)); // 报成功
        assert_eq!(fired.load(Ordering::SeqCst), 0, "must NOT killpg an already-exited task (pgid reuse)");
    }

    #[test]
    fn register_evicts_terminal_tasks_over_cap() {
        let reg = BackgroundRegistry::new();
        // 塞远超上限的已终止任务。
        for _ in 0..(MAX_TASKS + 50) {
            let t = BackgroundTask::new("x", Box::new(|| {}));
            t.set_terminal(TaskState::Exited(Some(0)));
            reg.register(t);
        }
        assert!(reg.list().len() <= MAX_TASKS, "terminal tasks must be evicted to bound growth");
    }
}
