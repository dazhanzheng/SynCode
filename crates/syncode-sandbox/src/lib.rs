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
/// Windows: Job Object —— 把 spawn 出的进程塞进 job, **关 job handle 即杀整棵进程树**
/// (`KILL_ON_JOB_CLOSE`), 外加活进程数 / 内存硬限。这是「跑模型给的命令」必须的底座:
/// `cargo test` 会 spawn `rustc`/test 子进程, 朴素 kill 会留孤儿; job 杀的是整树 (§7.4)。
///
/// 其他平台: 暂为 no-op (Linux 后端将走 cgroups + 进程组, §7); 调用方对直接子进程仍可 kill 兜底。
///
/// 用法: 先 [`ProcessContainer::new`], spawn 进程后立刻 [`contain`](Self::contain) 其原生 handle,
/// 超时/取消时 [`kill`](Self::kill) 整树; `Drop` 也会 (Windows) 杀树兜底。
pub struct ProcessContainer {
    #[cfg(windows)]
    job: windows_backend::JobObject,
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
        #[cfg(not(windows))]
        {
            let _ = policy;
            Ok(Self {})
        }
    }

    /// 把一个已 spawn 的 OS 进程 (原生 handle, Windows 上即 `HANDLE`) 纳入容器。
    /// 非 Windows 平台为 no-op。`handle` 以 `isize` 传递以保持跨平台签名一致。
    pub fn contain(&self, handle: isize) -> Result<(), SandboxError> {
        #[cfg(windows)]
        {
            self.job.assign(handle)
        }
        #[cfg(not(windows))]
        {
            let _ = handle;
            Ok(())
        }
    }

    /// 杀掉容器内整棵进程树 (Windows: `TerminateJobObject`)。非 Windows 为 no-op。
    pub fn kill(&self) {
        #[cfg(windows)]
        {
            self.job.kill();
        }
    }
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
