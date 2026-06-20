//! 安全/沙箱底座 (架构 §7)。
//!
//! 「激进授权的前提是先有这套底座」。骨架: 定义权限面 (`Policy`) 与给子进程上
//! 内核镣铐的抽象 (`Sandbox`); 各平台后端 (landlock/seccomp、Job Object、
//! sandbox_init) 随后填。当前提供一个 `NoopSandbox` 占位 (开发期)。
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox backend unsupported on this platform")]
    Unsupported,
    #[error("policy violation: {0}")]
    Policy(String),
    #[error("failed to apply sandbox: {0}")]
    Apply(String),
}

/// 一次受控执行的权限面 (capability surface)。默认收紧、显式放开 (§7.1)。
/// 模型能调的能力清单 = 它的权限边界 (§7.7)。
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub allow_network: bool,
    pub allow_exec: bool,
    /// 资源硬限 (Job Objects / cgroups, §7.4)。
    pub max_memory_bytes: Option<u64>,
    pub max_processes: Option<u32>,
    pub max_wall: Option<Duration>,
}

impl Policy {
    /// 最小权限: 啥也不放开。
    pub fn locked_down() -> Self {
        Self::default()
    }
}

/// 给 fork→exec 之间 / 子进程上「内核镣铐」的抽象 (§7)。平台后端各异 (§5.1 可移植税)。
pub trait Sandbox: Send + Sync {
    fn name(&self) -> &str;
    /// 在当前/子进程上施加 policy。须在 exec 前调用。
    fn apply(&self, policy: &Policy) -> Result<(), SandboxError>;
}

/// 占位实现: 不做任何隔离。仅开发期默认, 生产前必须换平台后端。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn name(&self) -> &str {
        "noop"
    }
    fn apply(&self, policy: &Policy) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// 进程容器 (spawn-into-container 模型, 区别于 `Sandbox::apply` 的 exec-前-施加模型)。
///
/// - **Windows**: Job Object —— spawn 后把进程 assign 进 job, **关 job handle 即杀整树**
///   (`KILL_ON_JOB_CLOSE`) + 活进程数/内存硬限。
/// - **Unix**: 进程组 —— 调用方 spawn 前给命令设 `process_group(0)` 让子进程自成组长, 超时
///   [`kill`](Self::kill) 时 `killpg(pgid, SIGKILL)` 杀整组 (Job Object 在 POSIX 的对位)。
///   ⚠️ **仅编译期 (cross-target check) 验证, 运行时待真 Linux/macOS 验**。资源硬限 / 能力面隔离
///   (cgroups/landlock/seccomp) 留给深度沙箱阶段。
///
/// 这是「跑模型给的命令」必须的底座: `cargo test` 会 spawn `rustc`/test 子进程, 朴素 kill 会留孤儿;
/// 容器杀的是整树 (§7.4)。
///
/// 用法: [`new`](Self::new) → spawn 后 [`contain`](Self::contain) (Windows 传 handle, Unix 传 pid)
/// → 超时/取消时 [`kill`](Self::kill)。
pub struct ProcessContainer {
    #[cfg(windows)]
    job: windows_backend::JobObject,
    #[cfg(unix)]
    pgid: std::sync::atomic::AtomicI32,
}

impl ProcessContainer {
    /// 按 `policy` 的资源硬限建一个容器。
    pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
        #[cfg(windows)]
        {
            Ok(Self {
                job: windows_backend::JobObject::new(policy)?,
            })
        }
        #[cfg(unix)]
        {
            let _ = policy;
            Ok(Self {
                pgid: std::sync::atomic::AtomicI32::new(0),
            })
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = policy;
            Ok(Self {})
        }
    }

    /// 把一个已 spawn 的进程纳入容器。Windows: 进程原生 `HANDLE`; Unix: 进程 `pid` (= 其进程组 pgid)。
    /// `id` 以 `isize` 传递以保持跨平台签名一致。
    pub fn contain(&self, id: isize) -> Result<(), SandboxError> {
        #[cfg(windows)]
        {
            self.job.assign(id)
        }
        #[cfg(unix)]
        {
            self.pgid.store(id as i32, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = id;
            Ok(())
        }
    }

    /// 杀掉容器内整棵进程树。Windows: `TerminateJobObject`; Unix: `killpg(pgid, SIGKILL)`。
    pub fn kill(&self) {
        #[cfg(windows)]
        {
            self.job.kill();
        }
        #[cfg(unix)]
        {
            let pgid = self.pgid.load(std::sync::atomic::Ordering::SeqCst);
            if pgid > 0 {
                unix_backend::kill_group(pgid);
            }
        }
    }
}

#[cfg(unix)]
mod unix_backend {
    //! Unix 进程组杀树 (`killpg`)。⚠️ 仅编译期 cross-check 验证, 运行时待真机验。
    /// 给整个进程组发 `SIGKILL` (子进程都已 `setpgid` 到同组, 故是杀整树)。
    /// 进程已退则 `ESRCH`, 无害。
    pub fn kill_group(pgid: i32) {
        unsafe {
            libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// **Unix 资源硬限**: 在子进程 (post-fork, pre-exec) 里施加 `setrlimit` —— 给 [`std::os::unix::process::CommandExt::pre_exec`]
/// 的闭包调用。`setrlimit` 是 async-signal-safe, 故可在 pre_exec 钩子里安全调用。
///
/// 当前只设 `RLIMIT_AS` (虚拟地址空间上限, 约束「单进程内存吃爆」), 仅在 `max_memory_bytes = Some` 时生效。
/// **进程数上限**故意不走 `RLIMIT_NPROC` —— 它是**按真实 uid** 全局计数的, 设低会误伤用户自己其它进程的
/// fork, 危险; per-job 的进程数硬限需 cgroups v2 (路线图 P2, 待 Linux 真机)。Windows 侧用 Job Object 的
/// `ActiveProcessLimit` (见 [`windows_backend`])。
///
/// ⚠️ 仅 cross-target 编译验证, 运行时待真 Linux/macOS 验。
#[cfg(unix)]
pub fn apply_rlimits(max_memory_bytes: Option<u64>) -> std::io::Result<()> {
    if let Some(bytes) = max_memory_bytes {
        let lim = libc::rlimit {
            rlim_cur: bytes as libc::rlim_t,
            rlim_max: bytes as libc::rlim_t,
        };
        // SAFETY: setrlimit 是 async-signal-safe; 在 pre_exec (单线程子进程) 中调用安全。
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &lim) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub mod windows_backend {
    //! Job Object 真实实现 (windows-sys FFI, §4/§7)。
    use super::{Policy, SandboxError};
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobObject {
        handle: *mut c_void,
    }
    // job handle 仅由本结构独占持有, 跨线程移动安全。
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(SandboxError::Apply("CreateJobObjectW failed".into()));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            // 关 job 即杀整树 —— 容器消亡时不留孤儿。
            let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(max) = policy.max_processes {
                info.BasicLimitInformation.ActiveProcessLimit = max;
                flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            }
            if let Some(mem) = policy.max_memory_bytes {
                info.JobMemoryLimit = mem as usize;
                flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            }
            info.BasicLimitInformation.LimitFlags = flags;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return Err(SandboxError::Apply("SetInformationJobObject failed".into()));
            }
            Ok(Self { handle })
        }

        pub fn assign(&self, process: isize) -> Result<(), SandboxError> {
            let ok = unsafe { AssignProcessToJobObject(self.handle, process as *mut c_void) };
            if ok == 0 {
                return Err(SandboxError::Apply("AssignProcessToJobObject failed".into()));
            }
            Ok(())
        }

        pub fn kill(&self) {
            unsafe { TerminateJobObject(self.handle, 1) };
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE: 关 handle 即杀整树, 兜底防孤儿。
            unsafe { CloseHandle(self.handle) };
        }
    }
}

// 平台后端 (TODO): 各写一个, 或砍成 Linux-only (见 §5.1 可移植税)。
#[cfg(target_os = "linux")]
pub mod linux {
    //! landlock + seccompiler + namespaces 后端 (TODO, §4/§7)。
}

#[cfg(target_os = "macos")]
pub mod macos {
    //! sandbox_init / Endpoint Security 后端 (TODO, §5.1)。
}
