//! 错误类型与重试分类 (指南 §16; 退避常数对照 CC `withRetry.ts`)。

use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// DeepSeek 返回的业务错误 (HTTP 4xx/5xx, §16)。
    #[error("deepseek api error: status={status} code={code:?} message={message}")]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
        /// `Retry-After` 头 (秒), 若服务器给出。重试时优先采纳 (见 [`backoff_delay`])。
        retry_after_secs: Option<u64>,
    },

    /// JSON 模式偶发空 content (指南 §10 已知问题)。
    #[error("response content was empty (json-mode known issue, guide §10)")]
    EmptyResponse,

    /// SSE/流式协议解析错误 (§8)。
    #[error("stream protocol error: {0}")]
    Stream(String),

    #[error("not implemented yet")]
    NotImplemented,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// 是否可重试 (退避)。对照 CC `shouldRetry`: 408 / 409 / 429 + 任意 5xx (含 529 overloaded) +
    /// 连接错误 / 超时。其余 (400 / 401 / 402 / 404 / 422 …) 需先修正, 不重试 (§16)。
    /// 注意 `finish_reason == InsufficientSystemResource` 也可重试, 但那是 finish_reason、不是 Error。
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Api { status, .. } => matches!(status, 408 | 409 | 429) || *status >= 500,
            Error::Http(e) => e.is_timeout() || e.is_connect(),
            _ => false,
        }
    }

    /// 服务器给出的 `Retry-After` (若有), 供退避时优先采纳。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Api { retry_after_secs: Some(s), .. } => Some(Duration::from_secs(*s)),
            _ => None,
        }
    }
}

/// 退避基准时延 (CC `BASE_DELAY_MS`)。
pub const BASE_RETRY_DELAY: Duration = Duration::from_millis(500);
/// 退避封顶 (CC `maxDelayMs` 默认)。
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(32);
/// 默认最大重试次数 (CC CLI `DEFAULT_MAX_RETRIES`; 交互式偏高, 脚本式应调低)。
pub const DEFAULT_MAX_RETRIES: u32 = 10;
/// `Retry-After` 仅在此窗口内被采纳, 超出则回落指数退避 (对照 CC 的 ≤60s 上限)。
const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);

/// 计算第 `attempt` 次重试前的退避时延 (`attempt` 从 1 起)。对照 CC `getRetryDelay`:
/// - `Retry-After` ∈ (0, 60]s → 直接采纳, **绕过**指数曲线 (服务器说了算);
/// - 否则 `min(500ms · 2^(attempt-1), 32s)` 再加 **0–25% 加性 jitter**。
///
/// `jitter01` ∈ [0,1) 由调用方注入 (便于测试; 生产传随机数)。
pub fn backoff_delay(attempt: u32, retry_after: Option<Duration>, jitter01: f64) -> Duration {
    if let Some(ra) = retry_after {
        if ra > Duration::ZERO && ra <= RETRY_AFTER_MAX {
            return ra;
        }
    }
    let shift = attempt.saturating_sub(1).min(30);
    let exp = BASE_RETRY_DELAY.saturating_mul(1u32 << shift);
    let base = exp.min(MAX_RETRY_DELAY);
    base + base.mul_f64(0.25 * jitter01.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> Error {
        Error::Api { status, code: None, message: String::new(), retry_after_secs: None }
    }

    #[test]
    fn retryable_classification() {
        for s in [408, 409, 429, 500, 502, 503, 529] {
            assert!(api(s).is_retryable(), "status {s} should retry");
        }
        for s in [400, 401, 402, 404, 422] {
            assert!(!api(s).is_retryable(), "status {s} should NOT retry");
        }
    }

    #[test]
    fn non_api_errors_not_retryable() {
        assert!(!Error::EmptyResponse.is_retryable());
        assert!(!Error::NotImplemented.is_retryable());
    }

    #[test]
    fn retry_after_within_window_bypasses_backoff() {
        let d = backoff_delay(1, Some(Duration::from_secs(5)), 0.0);
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn retry_after_out_of_window_falls_back_to_exponential() {
        // 120s 超出 60s 窗口 → 回落指数退避 (attempt 1, 无 jitter → 500ms)。
        let d = backoff_delay(1, Some(Duration::from_secs(120)), 0.0);
        assert_eq!(d, BASE_RETRY_DELAY);
    }

    #[test]
    fn exponential_growth_and_cap() {
        assert_eq!(backoff_delay(1, None, 0.0), Duration::from_millis(500));
        assert_eq!(backoff_delay(2, None, 0.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, None, 0.0), Duration::from_secs(2));
        // 远超封顶的 attempt → 钳到 32s。
        assert_eq!(backoff_delay(20, None, 0.0), MAX_RETRY_DELAY);
    }

    #[test]
    fn jitter_stays_within_25_percent() {
        let lo = backoff_delay(1, None, 0.0);
        let hi = backoff_delay(1, None, 1.0);
        assert_eq!(lo, Duration::from_millis(500));
        assert_eq!(hi, Duration::from_millis(625)); // 500 * 1.25
    }
}
