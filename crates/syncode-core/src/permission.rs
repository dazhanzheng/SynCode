//! 审批 / 权限骨架 (架构 §7.5, §10): 按「语义动作类别」授权, 而非逐条命令。

use thiserror::Error;

/// 语义动作类别 (借鉴 CC prompt-based 权限)。授权按类别一次性给, 而非逐命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionClass {
    ReadFs,
    WriteFs,
    RunTests,
    InstallDeps,
    Network,
    ArbitraryExec,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Error)]
#[error("action denied: {0:?}")]
pub struct Denied(pub ActionClass);

/// 审批器抽象。UI / 信任档位策略后续接入。
pub trait Approver: Send + Sync {
    fn decide(&self, action: &ActionClass) -> Decision;
}

/// 开发期默认: 全部放行。生产前必须替换为真实审批 (§7)。
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl Approver for AllowAll {
    fn decide(&self, action: &ActionClass) -> Decision {
        Decision::Allow
    }
}
