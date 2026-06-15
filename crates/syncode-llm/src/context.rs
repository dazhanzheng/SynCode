//! Context 裁切原语 (产品核心: 「完全掌控 context 裁切」, 架构 §1/§8)。
//!
//! 把指南 §7.4 / §7.5 / §12 的规则编码成对 `messages` 的就地操作。骨架: 签名/契约
//! 立起来, 实现为 `todo!()`。
//!
//! 关键不变量 (实现时必须守住):
//! - **配对结构**: 任何 `assistant.tool_calls[i].id` 必须有对应的 `role==Tool` 结果消息,
//!   不得产生孤儿 (否则序列非法 → 400)。裁切工具结果时只置存根 content, 不删消息。
//! - **§7.5 reasoning_content**: 在途 (未被新 user 关闭) 的 tool_calls 轮 → `Some("")`;
//!   已关闭的历史 tool_calls 轮 → `None` (整字段删)。
//! - **§12 前缀稳定**: 绝不改动可缓存的稳定前缀 (system / 工具定义), 以免破坏整前缀单元命中。

use crate::wire::Message;

/// 计算「在途推理跨度」: 自最近一条 `user` 消息 (不含) 到序列末尾的索引区间。
/// 该区间内带 `tool_calls` 的 assistant 轮受 §7.5 的 `Some("")` 规则约束。
pub fn inflight_span(messages: &[Message]) -> std::ops::Range<usize> {
    todo!("locate index range after the last user message (guide §7.5)")
}

/// 在途 tool_calls 轮: `reasoning_content = Some(String::new())`, 回收 CoT 但留字段过校验 (§7.5)。
pub fn reclaim_inflight_reasoning(messages: &mut [Message]) {
    todo!("set reasoning_content = Some(\"\") on open tool turns (guide §7.5)")
}

/// 已被后续 `user` 关闭的历史 tool_calls 轮: `reasoning_content = None` (整字段省略, §7.5)。
pub fn drop_closed_reasoning(messages: &mut [Message]) {
    todo!("set reasoning_content = None on closed tool turns (guide §7.5)")
}

/// 把工具结果 (`role==Tool`) 的 content 置存根 (如 `[cleared]`), 保留配对结构 (§7.5)。
pub fn stub_tool_results(messages: &mut [Message], marker: &str) {
    todo!("replace tool-result content with marker, keep tool_call<->tool pairing (guide §7.5)")
}

/// 删整轮 (最老的一组完整往返), 用于上下文超长回收。须保留稳定前缀 (§12)。
pub fn drop_oldest_round(messages: &mut Vec<Message>) {
    todo!("evict oldest complete round, preserve cacheable prefix (guide §12)")
}

/// 裁切策略: 每次请求前对待发送 `messages` 就地施加的步骤组合。
#[derive(Debug, Clone)]
pub struct TrimPolicy {
    /// 在途 tool_calls 轮的 reasoning_content 置 `""` 以回收 CoT。
    pub reclaim_inflight_cot: bool,
    /// 已关闭历史 tool_calls 轮的 reasoning_content 删字段。
    pub drop_closed_cot: bool,
    /// 工具结果 content 置存根。
    pub stub_tool_results: bool,
    /// 存根标记文本。
    pub tool_result_marker: String,
}

impl Default for TrimPolicy {
    fn default() -> Self {
        Self {
            reclaim_inflight_cot: true,
            drop_closed_cot: true,
            stub_tool_results: false,
            tool_result_marker: "[cleared]".to_string(),
        }
    }
}

impl TrimPolicy {
    /// 按既定顺序施加启用的裁切步骤, 全程保持 §12 前缀稳定。
    pub fn apply(&self, messages: &mut Vec<Message>) {
        todo!("apply enabled trimming steps in order (guide §7.5, §12)")
    }
}
