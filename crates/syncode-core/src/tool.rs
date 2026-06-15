//! 工具契约 (借鉴 Claude Code 设计 IP, 架构 §10)。
//!
//! 核心理念: **error message 是写给模型读的** —— 措辞为引导模型下一步, 不是给人看。

use async_trait::async_trait;
use serde_json::Value;
use syncode_llm::wire::{FunctionDef, ToolDef};
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

    /// 执行。`args` 是已从 `function.arguments` (JSON 字符串) 解析出的对象。
    async fn call(&self, args: Value) -> Result<ToolOutput, ToolError>;

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
