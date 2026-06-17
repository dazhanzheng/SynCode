//! 会话: 客户端自己累积完整 `messages` (接口无状态, §9)。

use syncode_llm::wire::{Message, Role};

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// 以一条 system 消息起头。system / 工具定义应固定在前缀以吃缓存 (§12)。
    pub fn with_system(system: impl Into<String>) -> Self {
        Self { messages: vec![Message::system(system)] }
    }

    /// 从既有消息序列重建 (resume: 从持久化 store 载入的 canonical 全文)。
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// 追加一条 user 消息 (开启新一轮, 关闭上一段在途工具跨度, §7.5)。
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(Message::user(content));
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// 最近一条 user 消息的下标 (用于计算在途推理跨度, §7.5)。
    pub fn last_user_index(&self) -> Option<usize> {
        self.messages.iter().rposition(|m| m.role == Role::User)
    }
}
