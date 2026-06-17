//! 工具契约 (借鉴 Claude Code 设计 IP, 架构 §10)。
//!
//! 核心理念: **error message 是写给模型读的** —— 措辞为引导模型下一步, 不是给人看。

use crate::file_state::FileStateCache;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use syncode_llm::wire::{FunctionDef, ToolDef};
use syncode_lsp::LspManager;
use thiserror::Error;

/// 工具执行错误。`Display` 文案直接回给模型, 故措辞要利于模型自纠偏。
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("execution failed: {0}")]
    Exec(String),
    #[error("permission denied: {0}")]
    Denied(String),
}

/// 工具一次调用的产出。`content` 是回给模型的字符串 (§11.1)。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    /// 是否为错误结果 (回给模型时可据此标注)。
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

/// 工具运行时上下文 (借鉴 CC `ToolUseContext`): 携带跨工具共享的状态。
#[derive(Clone)]
pub struct ToolCtx {
    /// 共享文件读状态缓存: Read 写、Edit/Write 读 —— 据此做「必先读」+ stale 检测 (§10)。
    pub files: Arc<FileStateCache>,
    /// 当前工作目录 (相对路径解析 / Grep 默认根)。
    pub cwd: PathBuf,
    /// 共享 LSP 客户端管理器: 跨工具调用复用常驻语言服务器 (§4 代码智能 / §6.2 机制③)。
    pub lsp: Arc<LspManager>,
}

impl ToolCtx {
    /// standalone / 测试用: 自带一个空的 LspManager。
    pub fn new(files: Arc<FileStateCache>, cwd: PathBuf) -> Self {
        Self::with_lsp(files, cwd, Arc::new(LspManager::new()))
    }

    /// agent loop 用: 注入**共享**的 LspManager, 全 turn 复用同一组常驻服务器 (持久活状态)。
    pub fn with_lsp(files: Arc<FileStateCache>, cwd: PathBuf, lsp: Arc<LspManager>) -> Self {
        Self { files, cwd, lsp }
    }
}

/// 工具契约。`Send + Sync` 以便放进 `Arc<dyn Tool>` 跨任务共享。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 稳定工具名 (进 `tool_calls.function.name`)。前缀稳定以吃缓存 (§12)。
    fn name(&self) -> &str;

    /// 写给模型读的描述。
    fn description(&self) -> &str;

    /// 参数 JSON Schema (§11)。建议满足 strict 三要素 (§11.4)。
    fn parameters(&self) -> Value;

    /// 是否高风险 (改系统 / 跑模型给的命令 / 碰网络): 需子进程 + 沙箱 + 审批 (§5.2)。
    fn is_dangerous(&self) -> bool {
        false
    }

    /// 执行。`args` 是已从 `function.arguments` (JSON 字符串) 解析出的对象;
    /// `ctx` 携带共享文件状态缓存等运行时上下文 (§10 文件工具联动)。
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;

    /// 生成发给模型的工具定义。
    fn to_def(&self) -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: FunctionDef {
                name: self.name().to_string(),
                description: Some(self.description().to_string()),
                parameters: self.parameters(),
                strict: false,
            },
        }
    }
}
