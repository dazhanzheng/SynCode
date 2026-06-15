//! 错误类型与重试分类 (指南 §16)。

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
    /// 是否可重试 (退避): 429 / 500 / 503 / 连接超时 (§16)。
    /// 注意 `finish_reason == InsufficientSystemResource` 也可重试, 但那不是 Error。
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Api { status, .. } => matches!(status, 429 | 500 | 503),
            Error::Http(e) => e.is_timeout() || e.is_connect(),
            _ => false,
        }
    }
}
