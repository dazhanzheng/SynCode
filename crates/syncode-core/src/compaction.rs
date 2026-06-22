//! 自动 context 压缩 (支柱 1): **稳定确定性的 N-轮 user-turn 滚动窗口** + 远端 LLM 结构化重组摘要兜底。
//!
//! 设计 (2026-06, 取代早先的「token 阈值 + 工具轮单位阶梯」):
//! - **不设 token 上限**: 投影 = 永远保最近 [`KEEP_RECENT_TURNS`] 个 **user-turn** 的原文 (CoT + tool
//!   result), 更旧的 turn **确定性地** 删 CoT + stub result。单位是 **user-turn 而非工具轮** —— 一个任务
//!   里可能有很多次 tool call, 按轮数留才不会在任务中途被裁掉上下文。
//! - **稳定 = 缓存友好**: 同一段历史每轮投影出**同样的字节**, 前缀缓存只在「某轮从完整滑成 stub」那一个
//!   边界破一次 (有界、可预测), 不像「临时连续裁」那样反复 thrash 缓存 (省的不如花的)。
//! - **窗口即界**: context 自稳定在 ≈ 最近 N 轮的大小, 增长合理可控、易管理。
//! - **trim-on-ingest 病态兜底**: 单个超大 result (> [`MAX_RESULT_CHARS`]) 即便在窗口内也头尾截断, 防
//!   一轮读 50 个文件把窗口撑爆。
//! - **LLM 摘要 (远端, 罕见)**: 仅当连「窗口投影」都超过真实窗口的 `summary_fraction` (5 轮巨大 / 跨任务)
//!   时点, 把旧 (已 stub) 前缀**结构化重组**成一段印象。canonical 全文永不删 (D1)。
//! - **recall 暂不做**: 旧细节要回, 重跑 tool 即可 (决策记录: 召回价值塌缩成贵/不可复现/时效对照的窄带,
//!   且不该跨结构化重组存活)。

use syncode_llm::context::{normalize_for_api, truncate_middle};
use syncode_llm::wire::{Message, Role};

/// 保留原文的最近 user-turn 数 (窗口大小)。窗口即界, 故无需 token 上限。
pub const KEEP_RECENT_TURNS: usize = 5;
/// 单个 tool result 在投影里的字符上限 (病态兜底: 一轮巨量输出也不撑爆窗口)。超出头尾截断。
pub const MAX_RESULT_CHARS: usize = 200_000;

/// 远端 LLM 摘要的预算 (仅用于「连窗口都太大」的高水位判断; 不再做 token-阈值阶梯)。
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// 模型上下文窗口 (token)。
    pub context_window: usize,
    /// 窗口投影超过 `context_window * summary_fraction` 才点 LLM 摘要 (罕见)。
    pub summary_fraction: f64,
}

impl Budget {
    /// 高水位 token: 窗口投影超过它才点 LLM 摘要。
    pub fn summary_high_water(&self) -> usize {
        (self.context_window as f64 * self.summary_fraction) as usize
    }
}

impl Default for Budget {
    /// DeepSeek v4 (1M 窗); 摘要在窗口投影超 ~800k 时才点 (5 轮巨大 / 跨任务, 极罕见)。
    fn default() -> Self {
        Self { context_window: 1_000_000, summary_fraction: 0.8 }
    }
}

/// 前导 system 前缀之后第一条消息的下标 (system/tools 前缀永不动, §12)。
pub fn first_non_system_index(msgs: &[Message]) -> usize {
    msgs.iter().position(|m| m.role != Role::System).unwrap_or(msgs.len())
}

/// 受保护尾部 (最近 `keep_turns` 个 user-turn) 的起点下标; user 不足 `keep_turns + 1` → `None` (全保留)。
/// 边界落在 user 消息上, 故其之前的 turn 都已被后续 user「关闭」(CoT 可安全置 None、不 400)。
pub fn protected_tail_start(msgs: &[Message], keep_turns: usize) -> Option<usize> {
    let user_idxs: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();
    if user_idxs.len() <= keep_turns {
        return None;
    }
    Some(user_idxs[user_idxs.len() - keep_turns])
}

/// 旧 (已关闭) turn 的 tool result 被 stub 成的占位 (带 teaser, 帮模型判断要不要重跑该 tool)。
fn stub_teaser(content: &str) -> String {
    let n = content.chars().count();
    let head: String = content.chars().take(80).collect::<String>().replace('\n', " ");
    format!("[earlier tool result trimmed ({n} chars; began: \"{head}…\") — re-run the tool if you need it]")
}

/// **稳定确定性 N-轮滚动窗口投影** (核心): 保最近 `keep_turns` 个 user-turn 的原文; 更旧的 turn 删 CoT +
/// stub result; 任何位置单个超大 result 头尾截断 (病态兜底)。末尾 normalize 兜底删孤儿。canonical 不动。
pub fn project_window(log: &[Message], keep_turns: usize) -> Vec<Message> {
    let mut out = log.to_vec();
    let boundary = protected_tail_start(log, keep_turns).unwrap_or(0);
    for (i, m) in out.iter_mut().enumerate() {
        if i < boundary {
            // 旧 (已关闭) turn: 删 CoT (closed → None 合法、不 400) + stub result。
            if m.role == Role::Assistant {
                m.reasoning_content = None;
            }
            if m.role == Role::Tool {
                if let Some(c) = &m.content {
                    m.content = Some(stub_teaser(c));
                }
            }
        } else if m.role == Role::Tool {
            // 窗口内: 原文保留, 但单个超大 result 头尾截断 (一轮巨量输出的兜底)。
            if let Some(c) = &m.content {
                if c.chars().count() > MAX_RESULT_CHARS {
                    m.content = Some(truncate_middle(c, MAX_RESULT_CHARS));
                }
            }
        }
    }
    normalize_for_api(&out)
}

/// 摘要present时的投影: `system 前缀` + `摘要(单条 user)` + `尾部的窗口投影`。末尾 normalize 兜底。
/// `boundary` = 摘要边界 (canonical 下标): `[first_non_system .. boundary)` 已被 `summary` 代表。
pub fn assemble_with_summary(
    log: &[Message],
    keep_turns: usize,
    summary: &Message,
    boundary: usize,
) -> Vec<Message> {
    let prefix_end = first_non_system_index(log);
    let boundary = boundary.clamp(prefix_end, log.len());
    let mut w: Vec<Message> = log[..prefix_end].to_vec();
    w.push(summary.clone());
    w.extend(project_window(&log[boundary..], keep_turns));
    normalize_for_api(&w)
}

/// 把一段 messages 摊平成可读文本喂给摘要器 (角色 + 文本 + 工具调用/结果摘要; CoT 不入摘要输入)。
pub fn flatten_for_summary(msgs: &[Message]) -> String {
    let mut s = String::new();
    for m in msgs {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool_result",
        };
        s.push_str(&format!("\n[{role}]\n"));
        if let Some(c) = &m.content {
            if !c.trim().is_empty() {
                s.push_str(c);
                s.push('\n');
            }
        }
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                s.push_str(&format!("-> call {}({})\n", tc.function.name, tc.function.arguments));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncode_llm::context::estimate_tokens;
    use syncode_llm::wire::{FunctionCall, ToolCall};

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

    /// system + n 个 turn (每 turn = user + 工具轮(带 CoT) + 结果)。
    fn turns(n: usize) -> Vec<Message> {
        let mut log = vec![Message::system("sys")];
        for i in 0..n {
            log.push(Message::user(format!("turn {i}")));
            log.push(asst_tool(&format!("c{i}"), &format!("reasoning {i}")));
            log.push(Message::tool_result(&format!("c{i}"), &format!("RESULT {i}")));
        }
        log
    }

    fn result_of(wire: &[Message], call: &str) -> Option<String> {
        wire.iter()
            .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(call))
            .and_then(|m| m.content.clone())
    }
    fn cot_of(wire: &[Message], call: &str) -> Option<Option<String>> {
        wire.iter()
            .find(|m| m.has_tool_calls() && m.tool_calls.as_ref().unwrap()[0].id == call)
            .map(|m| m.reasoning_content.clone())
    }

    #[test]
    fn under_keep_turns_keeps_everything_full() {
        let log = turns(3);
        let wire = project_window(&log, KEEP_RECENT_TURNS); // 3 <= 5 → 全保留
        assert_eq!(result_of(&wire, "c0").as_deref(), Some("RESULT 0"));
        assert_eq!(cot_of(&wire, "c0"), Some(Some("reasoning 0".to_string())));
    }

    #[test]
    fn keeps_recent_turns_full_trims_older() {
        let log = turns(8); // 8 个 user-turn, keep 5 → turn 0,1,2 旧; 3..7 在窗
        let wire = project_window(&log, 5);
        // 旧 turn 0: result stub + CoT 删。
        assert!(result_of(&wire, "c0").unwrap().starts_with("[earlier tool result trimmed"));
        assert_eq!(cot_of(&wire, "c0"), Some(None), "old closed-turn CoT must be dropped");
        // 窗口内 turn 7 (最近): result + CoT 原样。
        assert_eq!(result_of(&wire, "c7").as_deref(), Some("RESULT 7"));
        assert_eq!(cot_of(&wire, "c7"), Some(Some("reasoning 7".to_string())));
        // 窗口边界 turn 3: 在窗 → 原样。
        assert_eq!(result_of(&wire, "c3").as_deref(), Some("RESULT 3"));
    }

    #[test]
    fn most_recent_result_always_intact() {
        let log = turns(20);
        let wire = project_window(&log, 5);
        let recent = wire.iter().rev().find(|m| m.role == Role::Tool);
        assert_eq!(recent.and_then(|m| m.content.clone()).as_deref(), Some("RESULT 19"));
    }

    #[test]
    fn oversized_result_in_window_is_capped() {
        let mut log = turns(1);
        // turn 0 在窗 (只有 1 turn), 但其 result 巨大 → 头尾截断。
        if let Some(m) = log.iter_mut().find(|m| m.role == Role::Tool) {
            m.content = Some("X".repeat(MAX_RESULT_CHARS + 50_000));
        }
        let wire = project_window(&log, 5);
        let n = result_of(&wire, "c0").unwrap().chars().count();
        assert!(n <= MAX_RESULT_CHARS, "oversized window result must be capped, got {n}");
    }

    #[test]
    fn deterministic_same_history_same_bytes() {
        // 稳定性 = 缓存友好: 同一段历史投影两次, 字节一致。
        let log = turns(9);
        let a = project_window(&log, 5);
        let b = project_window(&log, 5);
        assert_eq!(estimate_tokens(&a), estimate_tokens(&b));
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.content, y.content);
            assert_eq!(x.reasoning_content, y.reasoning_content);
        }
    }

    #[test]
    fn structurally_valid_after_window() {
        let log = turns(10);
        let wire = project_window(&log, 5);
        assert_eq!(normalize_for_api(&wire).len(), wire.len(), "windowed wire must be structurally valid");
    }

    #[test]
    fn protected_tail_start_unit() {
        let log = turns(8);
        let b = protected_tail_start(&log, 5).expect("8 > 5");
        assert_eq!(log[b].role, Role::User);
        assert_eq!(log[b].content.as_deref(), Some("turn 3")); // 倒数第 5 个 user
        assert!(protected_tail_start(&turns(3), 5).is_none());
    }

    #[test]
    fn assemble_with_summary_keeps_prefix_and_window() {
        let log = turns(8);
        let boundary = protected_tail_start(&log, 5).unwrap();
        let summary = Message::user("[summary] older turns folded");
        let wire = assemble_with_summary(&log, 5, &summary, boundary);
        assert_eq!(wire[0].role, Role::System);
        assert_eq!(wire[1].role, Role::User);
        assert!(wire[1].content.as_deref().unwrap().contains("[summary]"));
        // 最近一轮仍在。
        let recent = wire.iter().rev().find(|m| m.role == Role::Tool);
        assert_eq!(recent.and_then(|m| m.content.clone()).as_deref(), Some("RESULT 7"));
        assert_eq!(normalize_for_api(&wire).len(), wire.len());
    }
}
