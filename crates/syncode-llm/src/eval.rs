//! Context 压缩**量化评估** harness (路线图 §5.4 / §12 开放问题「压缩质量评估方法」)。
//!
//! 「context 压缩质量是真正的上限杠杆」(README/§1) —— 但没有度量就无法优化。本模块给出一把**measuring
//! stick**: 对同一段对话 log 跑不同 [`TrimPolicy`], 量化每个策略的 **token 压缩率** 与 **保真代理指标**
//! (最近一轮工具结果是否原样保留、裁切后结构是否合法配对)。
//!
//! 用法: 攒一组真实任务的对话 log (Bash 解锁真任务后开始有真实多轮流量), 用 [`eval_suite`] 比较候选策略,
//! 选「压缩率高 + 保真不掉」的。语义摘要 (summarize-instead-of-drop) 等新策略落地后, 也用这把尺子评判。
//!
//! **诚实约束**: token 用 [`estimate_tokens`](crate::context::estimate_tokens) 估 (相对度量, 非精确
//! tokenizer); 「任务成功率」需真实跑 agent 才有, 不在此纯函数 harness 内 —— 这里量的是**可纯函数化**的那部分
//! (token + 结构 + 保真代理), 任务成功率作为上层 A/B 的另一半。

use crate::context::{estimate_tokens, normalize_for_api, TrimPolicy};
use crate::wire::{Message, Role};

/// 单个策略在某段 log 上的评估结果。
#[derive(Debug, Clone)]
pub struct TrimEval {
    pub label: String,
    /// 裁切前 token 估算 (已 normalize, 即真正会发送的基线)。
    pub tokens_before: usize,
    /// 裁切后 token 估算。
    pub tokens_after: usize,
    /// 压缩率 % = (before-after)/before*100。
    pub reduction_pct: f64,
    pub messages_before: usize,
    pub messages_after: usize,
    /// **保真代理**: 最近一轮工具结果的 content 是否原样保留 (压缩不该动最近轮)。
    pub recent_tool_result_intact: bool,
    /// **结构合法**: 裁切+normalize 后无孤儿 tool 结果 / 未配对 tool_calls (再过一遍 normalize 应幂等)。
    pub structurally_valid: bool,
}

/// 对一段 log 跑一个策略, 产出量化结果。
pub fn eval_policy(label: &str, log: &[Message], policy: &TrimPolicy) -> TrimEval {
    // 基线 = 不裁切、仅 normalize 后的发送态 (公平起点)。
    let baseline = normalize_for_api(log);
    let tokens_before = estimate_tokens(&baseline);

    let projected = policy.project(log);
    let wire = normalize_for_api(&projected);
    let tokens_after = estimate_tokens(&wire);

    let reduction_pct = if tokens_before == 0 {
        0.0
    } else {
        (tokens_before.saturating_sub(tokens_after) as f64) / tokens_before as f64 * 100.0
    };

    TrimEval {
        label: label.to_string(),
        tokens_before,
        tokens_after,
        reduction_pct,
        messages_before: baseline.len(),
        messages_after: wire.len(),
        recent_tool_result_intact: recent_tool_result_intact(log, &wire),
        structurally_valid: is_structurally_valid(&wire),
    }
}

/// 对一段 log 跑一组命名策略, 便于横向对比。
pub fn eval_suite(log: &[Message], policies: &[(String, TrimPolicy)]) -> Vec<TrimEval> {
    policies.iter().map(|(label, p)| eval_policy(label, log, p)).collect()
}

/// 一组标准候选策略 (现成可比): none / cot-only / stub / truncate-2k。
pub fn standard_suite() -> Vec<(String, TrimPolicy)> {
    vec![
        (
            "none".to_string(),
            TrimPolicy { keep_recent_cot_rounds: usize::MAX, reclaim_inflight_cot: false, ..Default::default() },
        ),
        ("cot-only".to_string(), TrimPolicy::default()),
        (
            "stub-old-results".to_string(),
            TrimPolicy { stub_tool_results: true, ..Default::default() },
        ),
        (
            "truncate-2k".to_string(),
            TrimPolicy { max_tool_result_chars: Some(2000), ..Default::default() },
        ),
    ]
}

/// 最近一轮工具结果的 content 在裁切后是否原样保留。
fn recent_tool_result_intact(log: &[Message], wire: &[Message]) -> bool {
    let Some(last) = log.iter().rev().find(|m| m.role == Role::Tool) else {
        return true; // 没有工具结果 → 无可破坏
    };
    let Some(id) = last.tool_call_id.as_deref() else { return true };
    match wire.iter().find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(id)) {
        Some(m) => m.content == last.content,
        None => false, // 最近一轮结果竟被删了 → 不保真
    }
}

/// 结构是否合法: 再过一遍 normalize 应**幂等** (没有可删的孤儿/未配对 → 已合法)。
fn is_structurally_valid(wire: &[Message]) -> bool {
    normalize_for_api(wire).len() == wire.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{FunctionCall, ToolCall};

    fn asst_tool(id: &str, reasoning: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: Some(String::new()),
            reasoning_content: Some(reasoning.to_string()),
            tool_calls: Some(vec![ToolCall {
                id: id.to_string(),
                kind: "function".to_string(),
                function: FunctionCall { name: "tool".to_string(), arguments: "{}".to_string() },
            }]),
            tool_call_id: None,
            name: None,
        }
    }

    /// 造一段多轮 log: system + 4 个工具轮 (每轮带长 reasoning + 长工具结果), 末尾一句回答。
    fn multi_round_log() -> Vec<Message> {
        let mut log = vec![Message::system("you are a coding agent")];
        for i in 0..4 {
            log.push(Message::user(format!("user turn {i}")));
            log.push(asst_tool(&format!("c{i}"), &"reasoning ".repeat(200)));
            log.push(Message::tool_result(&format!("c{i}"), &"BIG RESULT ".repeat(500)));
        }
        log.push(Message {
            role: Role::Assistant,
            content: Some("final answer".to_string()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        log
    }

    #[test]
    fn suite_reduces_tokens_and_preserves_recent_and_structure() {
        let log = multi_round_log();
        let results = eval_suite(&log, &standard_suite());

        let by = |name: &str| results.iter().find(|r| r.label == name).unwrap().clone();
        let none = by("none");
        let cot = by("cot-only");
        let stub = by("stub-old-results");
        let trunc = by("truncate-2k");

        // none: 不压缩 (基线 == 自身)。
        assert!(none.reduction_pct.abs() < 1e-9, "none should not reduce: {:?}", none);
        // 各压缩策略都应真的减少 token, 且压缩力度 stub >= truncate >= cot-only。
        assert!(cot.reduction_pct > 0.0, "cot-only should reclaim CoT: {cot:?}");
        assert!(stub.tokens_after < none.tokens_after, "stub should shrink");
        assert!(trunc.tokens_after < none.tokens_after, "truncate should shrink");
        assert!(stub.reduction_pct >= trunc.reduction_pct, "stub more aggressive than truncate");

        // 所有策略: 最近一轮工具结果原样保留 + 结构合法。
        for r in &results {
            assert!(r.recent_tool_result_intact, "{} broke the most recent tool result", r.label);
            assert!(r.structurally_valid, "{} produced an invalid structure", r.label);
        }
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s = "START".to_string() + &"x".repeat(5000) + "END";
        let out = crate::context::truncate_middle(&s, 100);
        assert!(out.starts_with("START"), "{out}");
        assert!(out.ends_with("END"), "{out}");
        assert!(out.chars().count() < 200, "should be compressed");
        assert!(out.contains("truncated"));
    }
}
