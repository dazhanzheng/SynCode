//! SynCode core.
//!
//! 自建 agent loop (非 Agent SDK, 架构 §1)。完全掌控 context 裁切与工具分发。
//! 模块:
//! - [`tool`]       : 工具契约 (借鉴 CC 设计 IP, §10)。
//! - [`registry`]   : 工具注册表 + 发给模型的工具定义。
//! - [`session`]    : 会话 (累积 messages, 接口无状态 §9)。
//! - [`context`]    : 每次请求前的裁切策略 (包装 llm::context, §7.5/§12)。
//! - [`permission`] : 按语义动作类别的审批骨架 (§7.5/§10)。
//! - [`prompt`]     : agent system prompt 的单一真相 (CLI/UI 同源, §2)。
//! - [`agent`]      : 自建 loop。
#![allow(dead_code, unused_variables)]

pub mod agent;
pub mod background;
pub mod compaction;
pub mod context;
pub mod file_state;
pub mod fs_scope;
pub mod pathutil;
pub mod permission;
pub mod prompt;
pub mod registry;
pub mod session;
pub mod state;
pub mod tool;

pub use agent::{AgentEvent, AgentLoop, AskGate, EventSink};
pub use background::{BackgroundRegistry, BackgroundTask, TaskState};
pub use context::ContextManager;
pub use file_state::{FileState, FileStateCache};
pub use fs_scope::{FsScope, SharedFsScope};
pub use prompt::system_prompt;
pub use registry::ToolRegistry;
pub use session::Session;
pub use tool::{FileDiff, Tool, ToolCtx, ToolError, ToolOutput};

/// 重导出 wire crate, 便于下游构造消息/工具定义。
pub use syncode_llm;
