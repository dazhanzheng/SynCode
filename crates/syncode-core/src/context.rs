//! 上下文管理: 每次请求前对会话 `messages` 施加裁切策略 (产品核心)。
//!
//! 薄包装 `syncode_llm::context` 的原语 (§7.5/§12)。

use syncode_llm::context::TrimPolicy;
use syncode_llm::wire::Message;

#[derive(Debug, Clone, Default)]
pub struct ContextManager {
    pub policy: TrimPolicy,
}

impl ContextManager {
    pub fn new(policy: TrimPolicy) -> Self {
        Self { policy }
    }

    /// 就地裁切待发送的 `messages` (回收在途 CoT、删已关闭 CoT、可选工具结果存根)。
    pub fn prepare(&self, messages: &mut Vec<Message>) {
        self.policy.apply(messages);
    }
}
