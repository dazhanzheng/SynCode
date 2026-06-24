//! Context 裁切原语 (产品核心: 「完全掌控 context 裁切」, 架构 §1/§8)。
//!
//! 把指南 §7.4 / §7.5 / §12 的规则编码成对 `messages` 的**纯**变换。设计基线 (D1):
//! full 原文 log 是唯一真相; 要发送的 wire `messages` 是它的**纯投影** —— [`TrimPolicy::project`]
//! 克隆后施加裁切, **不动 canonical**。
//!
//! 关键不变量 (2026-06-16/17 实测 + CC 源码对照):
//! - **配对结构**: 任何 `assistant.tool_calls[i].id` 必须有对应的 `role==Tool` 结果消息,
//!   不得产生孤儿 (否则序列非法 → 400)。裁切工具结果时只置存根 content, 不删消息;
//!   发送前由 [`normalize_for_api`] 兜底删孤儿 (借鉴 CC `normalizeMessagesForAPI`)。
//! - **reasoning_content (§7.4/§7.5)**: 按「距当前轮远近」分档 —— 最近 `keep_recent_cot_rounds`
//!   个工具轮保留完整 CoT (closed 工具轮 CoT 实测仍被喂给模型, 保留 = 推理连续性);
//!   更旧的: 在途轮 → `Some("")` (回收 token、过 400 校验), 已关闭轮 → `None` (整字段删)。
//!   置 `""` 在任意深度/位置都不 400, 故统一用它, 不依赖「首轮豁免」。
//! - **§12 前缀稳定**: 绝不改动可缓存的稳定前缀 (system / 工具定义)。

use crate::wire::{Message, Role};
use std::collections::HashSet;

/// 计算「在途推理跨度」: 自最近一条 `user` 消息 (不含) 到序列末尾的索引区间。
/// 该区间内带 `tool_calls` 的 assistant 轮: 第 2 个及以后必须带 reasoning_content (缺则 400),
/// 第一个豁免。统一置 `Some("")` 最稳。
pub fn inflight_span(messages: &[Message]) -> std::ops::Range<usize> {
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .map_or(0, |i| i + 1);
    start..messages.len()
}

/// 最近一条 `user` 消息的下标 (用于划分在途 / 已关闭跨度)。
fn last_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(|m| m.role == Role::User)
}

/// 带 `tool_calls` 的 assistant 轮 (= 工具轮) 的下标, 按出现顺序。
fn tool_round_indices(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Assistant && m.has_tool_calls())
        .map(|(i, _)| i)
        .collect()
}

/// 落在「保留窗口」内 (最近 `keep` 个工具轮) 的那些工具轮的 `tool_calls[].id` 集合。
fn kept_tool_call_ids(messages: &[Message], kept_rounds: &HashSet<usize>) -> HashSet<String> {
    let mut ids = HashSet::new();
    for &i in kept_rounds {
        if let Some(tcs) = messages.get(i).and_then(|m| m.tool_calls.as_ref()) {
            for tc in tcs {
                ids.insert(tc.id.clone());
            }
        }
    }
    ids
}

/// 删整轮 (最老的一组完整 user→…往返), 用于上下文超长回收。保留前导 system 前缀 (§12)。
/// 删除范围 = [第一条非 system 消息, 下一条 `user` 消息), 即最旧的一个完整用户轮。
pub fn drop_oldest_round(messages: &mut Vec<Message>) {
    let prefix_end = messages
        .iter()
        .position(|m| m.role != Role::System)
        .unwrap_or(messages.len());
    if prefix_end >= messages.len() {
        return; // 只有 system 前缀, 无可删
    }
    // prefix_end 处应是第一个用户轮的起点; 找下一个 user 作为该轮的终点。
    let next_user = messages
        .iter()
        .enumerate()
        .skip(prefix_end + 1)
        .find(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i);
    let end = next_user.unwrap_or(messages.len());
    messages.drain(prefix_end..end);
}

/// 发送前结构归一化 (③ choke point, 借鉴 CC `normalizeMessagesForAPI`)。**纯函数**, 不改入参。
///
/// - 删孤儿 `tool` 结果 (其 `tool_call_id` 在历史里找不到对应 `tool_calls`);
/// - 删「所有 `tool_calls` 都无结果」的 assistant 轮 (CC `filterUnresolvedToolUses`);
/// - 删空 assistant 轮 (无 content、无 reasoning、无 tool_calls)。
///
/// 注: 给「部分未配对」的孤儿 `tool_use` 合成 `is_error` 结果属于 loop 运行期修复 (③ 的另一半),
/// 不在此纯投影里做 —— 投影来源若是良构 log, 这里只当安全网。
pub fn normalize_for_api(messages: &[Message]) -> Vec<Message> {
    let call_ids: HashSet<&str> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.as_str())
        .collect();
    let result_ids: HashSet<&str> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();

    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let keep = match m.role {
            // 孤儿 tool 结果 (无对应 call) → 丢
            Role::Tool => m
                .tool_call_id
                .as_deref()
                .is_some_and(|id| call_ids.contains(id)),
            // 带 tool_calls 的 assistant: 至少一个 call 有结果才留
            Role::Assistant if m.has_tool_calls() => m
                .tool_calls
                .as_ref()
                .unwrap()
                .iter()
                .any(|tc| result_ids.contains(tc.id.as_str())),
            // 空 assistant 轮 → 丢
            Role::Assistant => {
                let no_content = m.content.as_deref().is_none_or(|c| c.trim().is_empty());
                let no_reasoning = m.reasoning_content.as_deref().is_none_or(str::is_empty);
                !(no_content && no_reasoning)
            }
            _ => true,
        };
        if keep {
            out.push(m.clone());
        }
    }
    out
}

/// 裁切策略: 每次请求前对待发送 `messages` 就地施加的步骤组合。
///
/// CoT 按「距当前轮远近」分档 (2026-06-16 实测: closed 工具轮 CoT 仍被喂给模型, 删 = 丢连续性):
/// 最近 `keep_recent_cot_rounds` 个工具轮保留完整 CoT; 更旧的回收 (在途置 `""` / 已关闭删字段)。
#[derive(Debug, Clone)]
pub struct TrimPolicy {
    /// 保留最近多少个工具轮的**完整** `reasoning_content` (推理连续性窗口)。
    /// `0` = 旧行为「凡可删就删」; `1` = 留最近一轮工具 CoT (默认)。
    pub keep_recent_cot_rounds: usize,
    /// 超出窗口的在途 tool_calls 轮: reasoning_content 置 `""` 回收 CoT (满足 400 校验)。
    pub reclaim_inflight_cot: bool,
    /// 工具结果 content 置存根 (只改超出窗口轮配对的 tool 结果, 保留配对结构)。
    pub stub_tool_results: bool,
    /// 存根标记文本。
    pub tool_result_marker: String,
    /// 比整体存根更**软**的压缩: 非保留窗口的工具结果若超过此字符数, 截成 head+tail (保留首尾、删中段)。
    /// `None` = 不截断。与 `stub_tool_results` 同开时 stub 优先 (更激进)。用 [`eval`](crate::eval) 量化对比。
    pub max_tool_result_chars: Option<usize>,
}

impl Default for TrimPolicy {
    fn default() -> Self {
        Self {
            keep_recent_cot_rounds: 1,
            reclaim_inflight_cot: true,
            stub_tool_results: false,
            tool_result_marker: "[Old tool result content cleared]".to_string(),
            max_tool_result_chars: None,
        }
    }
}

impl TrimPolicy {
    /// **纯投影** (D1): 从 full log 现算要发送的 wire `messages`, **不改 canonical**。
    pub fn project(&self, full_log: &[Message]) -> Vec<Message> {
        let mut out = full_log.to_vec();
        self.apply(&mut out);
        out
    }

    /// 就地施加裁切策略 (供 [`project`] 用; 全程保持 §12 前缀稳定)。
    ///
    /// [`project`]: TrimPolicy::project
    pub fn apply(&self, messages: &mut [Message]) {
        let rounds = tool_round_indices(messages);
        if rounds.is_empty() {
            return;
        }
        let keep_from = rounds.len().saturating_sub(self.keep_recent_cot_rounds);
        let kept: HashSet<usize> = rounds[keep_from..].iter().copied().collect();
        let last_user = last_user_index(messages);

        // 1) reasoning_content 保留窗口: 最近 N 轮留完整; 更旧者 inflight→Some("") / closed→None。
        for &i in &rounds {
            if kept.contains(&i) {
                continue;
            }
            let inflight = last_user.is_none_or(|lu| i > lu);
            if inflight {
                if self.reclaim_inflight_cot {
                    messages[i].reasoning_content = Some(String::new());
                }
            } else {
                messages[i].reasoning_content = None;
            }
        }

        // 2) 工具结果压缩: 超出保留窗口的工具轮所配对的 tool 结果 (保留配对结构, 只改 content)。
        //    stub_tool_results → 整体存根 (最激进); 否则 max_tool_result_chars → 软截断 head+tail。
        //    **工具结果窗口与 CoT 窗口解耦** (review fix #13): 至少保最近 1 个工具轮的结果不动 —— 否则
        //    keep_recent_cot_rounds=0 + stub/truncate 会把模型下一步必须用的最新结果也截掉。
        if self.stub_tool_results || self.max_tool_result_chars.is_some() {
            let result_keep_from = rounds.len().saturating_sub(self.keep_recent_cot_rounds.max(1));
            let result_kept: HashSet<usize> = rounds[result_keep_from..].iter().copied().collect();
            let kept_ids = kept_tool_call_ids(messages, &result_kept);
            for m in messages.iter_mut() {
                if m.role != Role::Tool {
                    continue;
                }
                let outside_window = m
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| !kept_ids.contains(id));
                if !outside_window {
                    continue;
                }
                if self.stub_tool_results {
                    m.content = Some(self.tool_result_marker.clone());
                } else if let Some(cap) = self.max_tool_result_chars {
                    if let Some(c) = &m.content {
                        if c.chars().count() > cap {
                            m.content = Some(truncate_middle(c, cap));
                        }
                    }
                }
            }
        }
    }
}

/// 把 `s` 压到约 `cap` 个字符: 保留 head + tail, 中段换成省略标记 (char-boundary 安全)。
/// 用于工具结果软压缩 —— 长结果的开头/结尾通常信息量最高, 中段可丢。
pub fn truncate_middle(s: &str, cap: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cap {
        return s.to_string();
    }
    let marker = "\n…[middle truncated]…\n";
    let marker_len = marker.chars().count();
    // cap 太小, 放不下 head+marker+tail → 硬截前 cap 个字符 (否则截断后**反而变长**, review fix #20)。
    if cap < marker_len + 2 {
        return chars[..cap].iter().collect();
    }
    let keep = cap - marker_len;
    let head = keep / 2;
    let tail = keep - head;
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_str}{marker}{tail_str}")
}

/// **token 估算** (§5.4 量化压缩的基础): 对一组 wire 消息估总 token 数。
/// 启发式 = 每条消息的 content/reasoning/tool_call 文本字节数之和 /4 + 每条固定 framing 开销。
/// 这是**相对**度量 (用于跨策略对比压缩率), 非精确 tokenizer —— 精确值由真 tokenizer 给, 但相对趋势一致。
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

fn estimate_message_tokens(m: &Message) -> usize {
    let mut bytes = 0usize;
    if let Some(c) = &m.content {
        bytes += c.len();
    }
    if let Some(r) = &m.reasoning_content {
        bytes += r.len();
    }
    if let Some(tcs) = &m.tool_calls {
        for tc in tcs {
            bytes += tc.function.name.len() + tc.function.arguments.len() + 8;
        }
    }
    // ~4 字节/token + 每条消息固定 framing 开销 (role 等)。
    bytes / 4 + 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{FunctionCall, ToolCall};

    fn user(c: &str) -> Message {
        Message::user(c)
    }
    fn sys(c: &str) -> Message {
        Message::system(c)
    }
    /// assistant 工具轮: 带一个 tool_call + 给定 reasoning_content。
    fn asst_tool(id: &str, reasoning: Option<&str>) -> Message {
        Message {
            role: Role::Assistant,
            content: Some(String::new()),
            reasoning_content: reasoning.map(str::to_string),
            tool_calls: Some(vec![ToolCall {
                id: id.to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "get_weather".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            tool_call_id: None,
            name: None,
        }
    }
    fn tool_res(id: &str, c: &str) -> Message {
        Message::tool_result(id, c)
    }
    fn asst_text(c: &str, reasoning: Option<&str>) -> Message {
        Message {
            role: Role::Assistant,
            content: Some(c.to_string()),
            reasoning_content: reasoning.map(str::to_string),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn inflight_span_after_last_user() {
        let msgs = vec![sys("s"), user("u1"), asst_tool("c1", Some("r")), tool_res("c1", "ok")];
        assert_eq!(inflight_span(&msgs), 2..4);
    }

    #[test]
    fn inflight_span_empty_when_user_is_last() {
        let msgs = vec![sys("s"), user("u1")];
        assert_eq!(inflight_span(&msgs), 2..2);
    }

    #[test]
    fn keep_window_keeps_recent_cot_drops_older_closed() {
        // 两个已关闭工具轮 (中间有 user 关闭), keep=1: 旧的 closed→None, 新的留完整。
        let mut msgs = vec![
            user("u1"),
            asst_tool("c1", Some("old reasoning")),
            tool_res("c1", "r1"),
            user("u2"), // 关闭第一轮
            asst_tool("c2", Some("new reasoning")),
            tool_res("c2", "r2"),
        ];
        let policy = TrimPolicy { keep_recent_cot_rounds: 1, ..Default::default() };
        policy.apply(&mut msgs);
        // c1 轮 (closed, 超窗) → reasoning_content 整字段删
        assert_eq!(msgs[1].reasoning_content, None);
        // c2 轮 (最近, 在窗) → 保留完整
        assert_eq!(msgs[4].reasoning_content.as_deref(), Some("new reasoning"));
    }

    #[test]
    fn inflight_nonkept_round_becomes_empty_string_not_none() {
        // 两个在途工具轮 (无新 user 关闭), keep=1: 旧的在途→Some(""), 新的留完整。
        // §7.5: 在途轮缺字段会 400; Some("") 满足校验且回收 CoT。
        let mut msgs = vec![
            user("u1"),
            asst_tool("c1", Some("first reasoning")),
            tool_res("c1", "r1"),
            asst_tool("c2", Some("second reasoning")),
            tool_res("c2", "r2"),
        ];
        let policy = TrimPolicy { keep_recent_cot_rounds: 1, ..Default::default() };
        policy.apply(&mut msgs);
        // c1 (在途, 超窗) → Some("") (非 None! 否则 400)
        assert_eq!(msgs[1].reasoning_content.as_deref(), Some(""));
        // c2 (最近, 在窗) → 完整
        assert_eq!(msgs[3].reasoning_content.as_deref(), Some("second reasoning"));
    }

    #[test]
    fn project_does_not_mutate_canonical() {
        let log = vec![user("u1"), asst_tool("c1", Some("keep me")), tool_res("c1", "r1"), user("u2"), asst_tool("c2", Some("recent")), tool_res("c2", "r2")];
        let policy = TrimPolicy::default();
        let wire = policy.project(&log);
        // canonical 原文不动
        assert_eq!(log[1].reasoning_content.as_deref(), Some("keep me"));
        // 投影里旧 closed 轮被删
        assert_eq!(wire[1].reasoning_content, None);
    }

    #[test]
    fn stub_tool_results_keeps_recent_full() {
        let mut msgs = vec![
            user("u1"),
            asst_tool("c1", Some("r")),
            tool_res("c1", "BIG OLD RESULT"),
            user("u2"),
            asst_tool("c2", Some("r")),
            tool_res("c2", "RECENT RESULT"),
        ];
        let policy = TrimPolicy { keep_recent_cot_rounds: 1, stub_tool_results: true, ..Default::default() };
        policy.apply(&mut msgs);
        assert_eq!(msgs[2].content.as_deref(), Some("[Old tool result content cleared]"));
        assert_eq!(msgs[5].content.as_deref(), Some("RECENT RESULT"));
    }

    #[test]
    fn drop_oldest_round_preserves_system_prefix() {
        let mut msgs = vec![
            sys("system prompt"),
            user("u1"),
            asst_text("a1", None),
            user("u2"),
            asst_text("a2", None),
        ];
        drop_oldest_round(&mut msgs);
        assert_eq!(msgs[0].role, Role::System); // 前缀保留
        assert_eq!(msgs[1].content.as_deref(), Some("u2")); // 第一轮被删, u2 上位
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn normalize_drops_orphan_tool_result() {
        let msgs = vec![user("u1"), tool_res("ghost", "orphan")];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
    }

    #[test]
    fn normalize_drops_unresolved_assistant_toolcall() {
        // assistant 发起 c1 但没有对应 tool 结果 → 丢该轮。
        let msgs = vec![user("u1"), asst_tool("c1", Some("r"))];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
    }

    #[test]
    fn normalize_keeps_paired_toolcall() {
        let msgs = vec![user("u1"), asst_tool("c1", Some("r")), tool_res("c1", "ok")];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn normalize_drops_empty_assistant() {
        let msgs = vec![user("u1"), asst_text("   ", None), asst_text("real answer", None)];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].content.as_deref(), Some("real answer"));
    }

    #[test]
    fn tool_result_window_protects_last_round_even_with_cot_zero() {
        // keep_recent_cot_rounds=0 + stub: 旧轮结果存根, 但最新轮结果必须原样 (review fix #13)。
        let mut msgs = vec![
            user("u1"),
            asst_tool("c1", Some("r")),
            tool_res("c1", "OLD RESULT"),
            user("u2"),
            asst_tool("c2", Some("r")),
            tool_res("c2", "RECENT RESULT"),
        ];
        let policy =
            TrimPolicy { keep_recent_cot_rounds: 0, stub_tool_results: true, ..Default::default() };
        policy.apply(&mut msgs);
        assert_eq!(msgs[2].content.as_deref(), Some("[Old tool result content cleared]"));
        assert_eq!(msgs[5].content.as_deref(), Some("RECENT RESULT"), "latest result must survive");
    }

    #[test]
    fn truncate_middle_small_cap_does_not_expand() {
        // cap 太小放不下 marker → 硬截, 绝不让结果变长 (review fix #20)。
        let s = "x".repeat(6);
        let out = truncate_middle(&s, 5);
        assert!(out.chars().count() <= 5, "got {} chars: {out}", out.chars().count());
    }
}
