//! 自建 agent loop (非 Agent SDK, 架构 §1)。

use crate::context::ContextManager;
use crate::permission::{AllowAll, Approver};
use crate::registry::ToolRegistry;
use crate::session::Session;
use std::sync::Arc;
use syncode_llm::client::{DeepSeekClient, MODEL};
use syncode_llm::wire::{ChatRequest, ReasoningEffort, Thinking, ThinkingType};

/// 一个 turn 的自建循环: 裁切 → 请求 → 若 `tool_calls` 则分发执行 → 回填 → 直到 `stop`。
pub struct AgentLoop {
    client: Arc<DeepSeekClient>,
    tools: ToolRegistry,
    context: ContextManager,
    approver: Arc<dyn Approver>,
}

impl AgentLoop {
    pub fn new(client: Arc<DeepSeekClient>, tools: ToolRegistry) -> Self {
        Self {
            client,
            tools,
            context: ContextManager::default(),
            approver: Arc::new(AllowAll),
        }
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    pub fn with_context(mut self, context: ContextManager) -> Self {
        self.context = context;
        self
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// 组装一次请求: 思考模式 + max 强度 (复杂 agent 场景, §7.1), 带全部工具定义。
    fn build_request(&self, session: &Session) -> ChatRequest {
        ChatRequest {
            model: MODEL.to_string(),
            messages: session.messages().to_vec(),
            thinking: Some(Thinking { kind: ThinkingType::Enabled }),
            reasoning_effort: Some(ReasoningEffort::Max),
            tools: Some(self.tools.definitions()),
            tool_choice: None,
            max_tokens: Some(8192),
            temperature: None,
            stop: None,
            stream: false,
            response_format: None,
        }
    }

    /// 跑一个完整 turn 直到模型给出 `finish_reason == Stop`。
    ///
    /// TODO 实现要点:
    /// - 每次请求前 `context.prepare(&mut messages)` 裁切 (§7.5/§12);
    /// - 遵守 §7.4 reasoning_content 回传规则 (有 tool_calls → 在途轮必须带, 见 wire/context);
    /// - 处理一轮多个 `tool_calls` (并行执行, §11);
    /// - 加「tool-call 漏进 content」恢复守卫 (§8 坑#2, DeepSeek #1244);
    /// - 危险工具走 `approver` 审批 (§7)。
    pub async fn run_turn(&mut self, session: &mut Session) -> syncode_llm::Result<()> {
        todo!("self-built loop: trim ctx -> client.chat -> dispatch tool_calls -> append -> until Stop")
    }
}
