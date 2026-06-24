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

    /// 中断后修复历史, 使其**合法且可继续** (借鉴 Claude Code 的中断处理):
    /// 给每个**没有配对 tool_result** 的 assistant `tool_call` 补一条「中断」结果 (否则下次请求会 400),
    /// 再压一条 `[Request interrupted by user]` user 标记 (让模型知道这里被人打断)。
    /// 用户按 Stop 中止半截 turn 后调用 —— 之后照常 push_user 继续即可。
    pub fn repair_after_interrupt(&mut self) {
        use std::collections::HashSet;
        // 已被回答的 tool_call id。
        let answered: HashSet<String> = self
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        // 收集所有悬空 (无结果) 的 tool_call id。
        let dangling: Vec<String> = self
            .messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flatten()
            .map(|tc| tc.id.clone())
            .filter(|id| !answered.contains(id))
            .collect();
        for id in dangling {
            self.messages.push(Message::tool_result(id, "Interrupted by user"));
        }
        self.push_user("[Request interrupted by user]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncode_llm::wire::{FunctionCall, ToolCall};

    fn assistant_tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: Some(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: id.to_string(),
                kind: "function".to_string(),
                function: FunctionCall { name: "Bash".to_string(), arguments: "{}".to_string() },
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn repair_pairs_dangling_tool_calls_and_marks_interrupt() {
        let mut s = Session::with_system("sys");
        s.push_user("do it");
        s.push(assistant_tool_call("call_1")); // 悬空: 无 tool_result
        s.repair_after_interrupt();
        let tool_results: Vec<_> = s.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1, "one synthetic tool_result for the dangling call");
        assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("call_1"));
        let last = s.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.as_deref(), Some("[Request interrupted by user]"));
    }

    #[test]
    fn repair_does_not_double_answer_already_answered_calls() {
        let mut s = Session::with_system("sys");
        s.push(assistant_tool_call("call_1"));
        s.push(Message::tool_result("call_1", "done")); // 已回答
        s.repair_after_interrupt();
        let tool_results: Vec<_> = s.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1, "must not add a second result for an answered call");
        assert_eq!(tool_results[0].content.as_deref(), Some("done"));
    }
}
