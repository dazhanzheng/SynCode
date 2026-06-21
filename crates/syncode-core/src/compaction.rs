//! 自动、预算触发的 context 压缩控制器 (支柱 1 核心: 「完全掌控 context 裁切」)。
//!
//! per-request 的投影裁切 (`llm::context`) 只是**原语**; 本模块是缺失的那个**控制器**:
//! 度量预算 → 按**信息损失递增**的阶梯升级裁切。设计基线:
//! - 全程只算/改 **wire 投影**; canonical 全文 log 永不动 (D1)。
//! - 压力 ≤ `soft_threshold` → 维持配置的默认投影 (今天的行为, **cache-stable**, 不重写可缓存前缀)。
//! - 超预算才爬阶梯; 每升一档都会重写可缓存前缀 → 必然 cache miss, 故**一步到位、批量**升, 不每轮 dribble。
//! - 每档都映射到**现有** [`TrimPolicy`] 旋钮, 高档 subsume 低档 (单调)。`TrimPolicy::apply` 里
//!   「最近工具结果窗口钳到 ≥1」已保证最新工具结果在任何档都不被动。
//!
//! LLM 摘要 (顶档) 见 [`crate::agent`] 的 `summarize` 路径 —— 仅当结构阶梯压不到 `hard_threshold` 时点燃。

use syncode_llm::context::{drop_oldest_round, estimate_tokens, normalize_for_api, TrimPolicy};
use syncode_llm::wire::{Message, Role};

/// 受保护的「最近 user 轮」数: 这么多个最新轮**永不**被结构性删除 (DropOldest 档的下限)。
pub const PROTECTED_TAIL_SPANS: usize = 3;

/// 预算账本: 镜像 CC autoCompact 的阈值数学, 按 DeepSeek 窗口尺寸调。
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// 模型上下文窗口 (token)。
    pub context_window: usize,
    /// 给模型自身 completion + CoT 预留。
    pub output_reserve: usize,
    /// 跑一次 LLM 摘要所需的 headroom。
    pub summary_reserve: usize,
    /// 给 estimate_tokens 低估留的 slack。
    pub safety_buffer: usize,
}

impl Budget {
    /// 软触发: 超过它就在发送前跑结构阶梯。
    pub fn soft_threshold(&self) -> usize {
        self.context_window
            .saturating_sub(self.output_reserve + self.safety_buffer)
    }
    /// 硬触发: 结构阶梯都压不下去 → 该上 LLM 摘要顶档。
    pub fn hard_threshold(&self) -> usize {
        self.soft_threshold().saturating_sub(self.summary_reserve)
    }
}

impl Default for Budget {
    /// DeepSeek v4 (保守取 128K 窗)。soft = 128000-16000-13000 = 99000; hard = 79000。
    fn default() -> Self {
        Self {
            context_window: 128_000,
            output_reserve: 16_000,
            summary_reserve: 20_000,
            safety_buffer: 13_000,
        }
    }
}

/// 压缩阶梯档位, 按信息损失**单调递增**。`Ord` 派生用于「不低于某 floor」比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// 配置的默认投影 (= 今天的行为): keep_recent_cot=1 + 回收在途 CoT。cache-stable。
    Baseline = 0,
    /// 删**所有闭合轮** CoT (keep_recent_cot_rounds=0; 在途轮仍 `Some("")` 不 400)。
    ReclaimCot,
    /// 旧工具结果头尾软截断 (max_tool_result_chars)。
    SoftTruncate,
    /// 旧工具结果体替成 marker (stub)。
    StubResults,
    /// 结构性删最旧的完整 user 轮 (护住最近 `PROTECTED_TAIL_SPANS` 轮)。
    DropOldest,
}

impl Rung {
    pub fn label(self) -> &'static str {
        match self {
            Rung::Baseline => "baseline",
            Rung::ReclaimCot => "reclaim-cot",
            Rung::SoftTruncate => "soft-truncate",
            Rung::StubResults => "stub-results",
            Rung::DropOldest => "drop-oldest",
        }
    }
}

/// 一次压缩决策的产物。
#[derive(Debug, Clone)]
pub struct Compacted {
    /// 选中的档位。
    pub rung: Rung,
    /// 现算出的待发送 wire messages。
    pub wire: Vec<Message>,
    /// 压缩前的 token 估算 (baseline 投影)。
    pub before: usize,
    /// 压缩后的 token 估算。
    pub after: usize,
}

/// 把一个档位翻译成具体 [`TrimPolicy`] (从配置的 `base` 起, 高档叠加更激进的旋钮)。
fn policy_for(base: &TrimPolicy, rung: Rung) -> TrimPolicy {
    let mut p = base.clone();
    if rung >= Rung::ReclaimCot {
        p.keep_recent_cot_rounds = 0;
    }
    if rung >= Rung::SoftTruncate {
        // 取更激进者 (若 base 已配了更小的上限, 保留之)。
        p.max_tool_result_chars = Some(p.max_tool_result_chars.unwrap_or(2_000).min(2_000));
    }
    if rung >= Rung::StubResults {
        p.stub_tool_results = true;
    }
    p
}

/// 删最旧的一个完整 user 轮, 但**护住最近 `keep_spans` 个 user 轮**。返回是否真的删了
/// (false = 已到保护边界, 无可删)。在 clone 上调用 —— canonical 永不动 (D1)。
fn drop_oldest_above_floor(messages: &mut Vec<Message>, keep_spans: usize) -> bool {
    let user_count = messages.iter().filter(|m| m.role == Role::User).count();
    if user_count <= keep_spans {
        return false;
    }
    let before = messages.len();
    drop_oldest_round(messages);
    messages.len() < before
}

/// 选**能装进 `target` 的最低档** (纯函数, 零模型成本, 零延迟)。`floor` = 反应式兜底强制的下限档。
fn climb_structural(base: &TrimPolicy, log: &[Message], target: usize, floor: Rung) -> (Rung, Vec<Message>) {
    for rung in [Rung::Baseline, Rung::ReclaimCot, Rung::SoftTruncate, Rung::StubResults] {
        if rung < floor {
            continue;
        }
        let wire = normalize_for_api(&policy_for(base, rung).project(log));
        if estimate_tokens(&wire) <= target {
            return (rung, wire);
        }
    }
    // 到 StubResults 仍超 → 结构删最旧轮循环 (护住保护尾)。
    let mut projected = policy_for(base, Rung::StubResults).project(log);
    loop {
        let wire = normalize_for_api(&projected);
        if estimate_tokens(&wire) <= target {
            return (Rung::DropOldest, wire);
        }
        if !drop_oldest_above_floor(&mut projected, PROTECTED_TAIL_SPANS) {
            // 已到保护边界: 尽力而为 (剩下的交给 LLM 摘要顶档 / 反应式重试)。
            return (Rung::DropOldest, normalize_for_api(&projected));
        }
    }
}

/// 控制器入口: 给定 canonical `log` + 预算 + 上一轮 server 的精确 `measured` prompt_tokens (可选)
/// + 反应式 `floor` (默认 `Baseline`), 现算要发送的 wire messages。
///
/// 压力 = max(当前 baseline 投影的 estimate, 上一轮 server prompt_tokens)。前者抓「log 又长了」,
/// 后者用 server 精确值纠正 estimate 的系统性低估。压力 ≤ soft 且 floor=Baseline → 维持今天的行为。
pub fn compact_for_send(
    base: &TrimPolicy,
    log: &[Message],
    budget: &Budget,
    measured: Option<usize>,
    floor: Rung,
) -> Compacted {
    let baseline = normalize_for_api(&base.project(log));
    let before = estimate_tokens(&baseline);
    let pressure = before.max(measured.unwrap_or(0));
    let target = budget.soft_threshold();

    if pressure <= target && floor == Rung::Baseline {
        return Compacted { rung: Rung::Baseline, after: before, wire: baseline, before };
    }
    // estimate 系统性低估: 若 server 的精确读数 `pressure` 高于本地 `before` 估算, 说明估算偏小 factor
    // = pressure/before。把结构阶梯的 target 按同比例**收紧**, 让「estimate(wire) ≤ scaled」等价于
    // 「真实 tokens ≤ target」—— 用 server 精确值纠偏, 而非盲信 estimate (u128 防中间溢出)。
    let scaled_target = if pressure > before && before > 0 {
        ((target as u128 * before as u128) / pressure as u128) as usize
    } else {
        target
    };
    let (rung, wire) = climb_structural(base, log, scaled_target, floor);
    let after = estimate_tokens(&wire);
    Compacted { rung, wire, before, after }
}

/// 下一个更激进的档 (反应式兜底用)。已到顶 (DropOldest) 返回 None。
pub fn next_rung(rung: Rung) -> Option<Rung> {
    match rung {
        Rung::Baseline => Some(Rung::ReclaimCot),
        Rung::ReclaimCot => Some(Rung::SoftTruncate),
        Rung::SoftTruncate => Some(Rung::StubResults),
        Rung::StubResults => Some(Rung::DropOldest),
        Rung::DropOldest => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncode_llm::wire::{FunctionCall, Role, ToolCall};

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

    /// system + N 个工具轮 (每轮长 reasoning + 长结果) + 末尾回答。
    fn big_log(rounds: usize) -> Vec<Message> {
        let mut log = vec![Message::system("you are a coding agent")];
        for i in 0..rounds {
            log.push(Message::user(format!("user turn {i}")));
            log.push(asst_tool(&format!("c{i}"), &"reasoning ".repeat(300)));
            log.push(Message::tool_result(&format!("c{i}"), &"BIG RESULT ".repeat(800)));
        }
        log.push(Message::user("latest".to_string()));
        log.push(asst_tool("clast", &"recent reasoning".repeat(50)));
        log.push(Message::tool_result("clast", "RECENT RESULT keep me intact"));
        log
    }

    fn tiny_budget(window: usize) -> Budget {
        Budget { context_window: window, output_reserve: 0, summary_reserve: 0, safety_buffer: 0 }
    }

    #[test]
    fn under_budget_stays_baseline_and_cache_stable() {
        let log = big_log(2);
        // 超大窗口 → 压力远低于 soft → Baseline。
        let c = compact_for_send(&TrimPolicy::default(), &log, &Budget::default(), None, Rung::Baseline);
        assert_eq!(c.rung, Rung::Baseline);
    }

    #[test]
    fn over_budget_climbs_and_shrinks() {
        let log = big_log(6);
        let baseline_tokens = estimate_tokens(&normalize_for_api(&TrimPolicy::default().project(&log)));
        // 把窗口设在 baseline 的一半 → 必须爬阶梯压下来。
        let budget = tiny_budget(baseline_tokens / 2);
        let c = compact_for_send(&TrimPolicy::default(), &log, &budget, None, Rung::Baseline);
        assert_ne!(c.rung, Rung::Baseline, "should have climbed off baseline");
        assert!(c.after < c.before, "compaction must shrink: {} -> {}", c.before, c.after);
        assert!(c.after <= budget.soft_threshold() || c.rung == Rung::DropOldest,
            "should fit under target unless it hit the protected-tail floor");
    }

    #[test]
    fn most_recent_tool_result_survives_every_rung() {
        let log = big_log(6);
        let budget = tiny_budget(50); // 极紧 → 一路爬到 DropOldest
        let c = compact_for_send(&TrimPolicy::default(), &log, &budget, None, Rung::Baseline);
        let recent = c.wire.iter().rev().find(|m| m.role == Role::Tool);
        assert_eq!(
            recent.and_then(|m| m.content.as_deref()),
            Some("RECENT RESULT keep me intact"),
            "newest tool result must never be stubbed/dropped (rung={:?})", c.rung
        );
    }

    #[test]
    fn server_measured_overrides_low_estimate() {
        let log = big_log(1);
        let budget = Budget::default();
        // estimate 很小 → 本会 Baseline; 但 server 说 prompt 已 110k (> soft 99k) → 强制爬阶梯。
        let c = compact_for_send(&TrimPolicy::default(), &log, &budget, Some(110_000), Rung::Baseline);
        assert_ne!(c.rung, Rung::Baseline, "server-measured pressure must trigger compaction");
    }

    #[test]
    fn reactive_floor_forces_minimum_rung() {
        let log = big_log(2);
        // 窗口够大本会 Baseline, 但 floor 强制 ≥ StubResults (反应式兜底场景)。
        let c = compact_for_send(&TrimPolicy::default(), &log, &Budget::default(), None, Rung::StubResults);
        assert!(c.rung >= Rung::StubResults, "floor must pin the minimum rung, got {:?}", c.rung);
    }

    #[test]
    fn structurally_valid_after_compaction() {
        let log = big_log(6);
        let budget = tiny_budget(80);
        let c = compact_for_send(&TrimPolicy::default(), &log, &budget, None, Rung::Baseline);
        // 再过一遍 normalize 应幂等 (无孤儿 / 未配对)。
        assert_eq!(normalize_for_api(&c.wire).len(), c.wire.len(), "compacted wire must be structurally valid");
    }
}
