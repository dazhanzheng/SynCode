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

// 平台后端 (TODO): 各写一个, 或砍成 Linux-only (见 §5.1 可移植税)。
#[cfg(target_os = "linux")]
pub mod linux {
    //! landlock + seccompiler + namespaces 后端 (TODO, §4/§7)。
}

#[cfg(windows)]
pub mod windows_backend {
    //! Job Objects + 受限令牌后端 (TODO, §4/§7)。
}

#[cfg(target_os = "macos")]
pub mod macos {
    //! sandbox_init / Endpoint Security 后端 (TODO, §5.1)。
}
