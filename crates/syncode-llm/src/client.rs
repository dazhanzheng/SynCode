//! DeepSeek HTTP client (§3, §6): 收发 (① transport) + 确定性退避重试 (②)。

use crate::error::{backoff_delay, Error, Result, DEFAULT_MAX_RETRIES};
use crate::stream::{ChatStreamChunk, StreamAccumulator};
use crate::wire::{ChatRequest, ChatResponse};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// 标准 OpenAI 兼容入口 (§3)。
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
/// Beta 入口 (prefix / FIM / strict 工具, §3)。
pub const BETA_BASE_URL: &str = "https://api.deepseek.com/beta";
/// 本项目唯一使用的模型 (§4)。
pub const MODEL: &str = "deepseek-v4-pro";

/// client 配置。API Key 务必经环境变量注入, 切勿硬编码 (§1.1 安全提示)。
#[derive(Debug, Clone)]
pub struct DeepSeekConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl DeepSeekConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            model: MODEL.to_string(),
        }
    }

    /// 从环境装配 (`DEEPSEEK_API_KEY`)。
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").map_err(|_| Error::Api {
            status: 401,
            code: Some("missing_api_key".to_string()),
            message: "DEEPSEEK_API_KEY environment variable is not set".to_string(),
            retry_after_secs: None,
        })?;
        Ok(Self::new(api_key))
    }
}

/// DeepSeek 对话补全 client。
pub struct DeepSeekClient {
    config: DeepSeekConfig,
    http: reqwest::Client,
    max_retries: u32,
}

impl DeepSeekClient {
    pub fn new(config: DeepSeekConfig) -> Result<Self> {
        let http = reqwest::Client::builder().build()?;
        Ok(Self { config, http, max_retries: DEFAULT_MAX_RETRIES })
    }

    /// 覆盖最大重试次数 (脚本式宜低, 交互式宜高, 对照 CC 的 10)。
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn config(&self) -> &DeepSeekConfig {
        &self.config
    }

    /// 非流式对话补全 + 确定性退避重试 (②)。可重试错误 (408/409/429/5xx/连接) 退避后重发;
    /// 其余 (400/401/402/422…) 立即返回 (§16)。
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.chat_once(request).await {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt <= self.max_retries && e.is_retryable() => {
                    let delay = backoff_delay(attempt, e.retry_after(), pseudo_jitter());
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 单次请求 (① transport): 序列化 → POST → 反序列化; 失败按 HTTP 状态码分类成 typed [`Error`]。
    /// 非流式下 keep-alive 表现为响应体前导空行, `serde_json` 解析时自动跳过 (§8)。
    pub async fn chat_once(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.config.api_key)
            .json(request)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            let body = resp.text().await.unwrap_or_default();
            let (code, message) = parse_api_error(&body);
            return Err(Error::Api {
                status: status.as_u16(),
                code,
                message: if message.is_empty() { truncate(&body, 500) } else { message },
                retry_after_secs: retry_after,
            });
        }

        Ok(resp.json::<ChatResponse>().await?)
    }

    /// 流式对话补全 (§8): SSE 分片重组 (§11.5); 失败时回落非流式 [`chat`] (§8.2 streaming→non-streaming)。
    ///
    /// [`chat`]: DeepSeekClient::chat
    pub async fn chat_stream(&self, request: &ChatRequest) -> Result<ChatResponse> {
        match self.stream_inner(request, |_| {}).await {
            Ok(resp) => Ok(resp),
            Err(_) => self.chat(request).await, // streaming→非流式 fallback
        }
    }

    /// 流式 + **逐 chunk 回调** (供 UI 逐字渲染 / token 实时计数): 每收到一个 SSE chunk 调一次
    /// `on_chunk`, 最终仍重组成与非流式等价的 [`ChatResponse`]。流式失败 → 回落非流式 [`chat`]
    /// (此时不再有 delta 回调, 但仍拿到完整结果)。
    pub async fn chat_streaming<F: FnMut(&ChatStreamChunk)>(
        &self,
        request: &ChatRequest,
        on_chunk: F,
    ) -> Result<ChatResponse> {
        match self.stream_inner(request, on_chunk).await {
            Ok(resp) => Ok(resp),
            Err(_) => self.chat(request).await,
        }
    }

    async fn stream_inner<F: FnMut(&ChatStreamChunk)>(
        &self,
        request: &ChatRequest,
        mut on_chunk: F,
    ) -> Result<ChatResponse> {
        use futures_util::StreamExt;
        let mut req = request.clone();
        req.stream = true;
        req.stream_options = Some(serde_json::json!({ "include_usage": true }));

        let url = format!("{}/chat/completions", self.config.base_url);
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.config.api_key)
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let (code, message) = parse_api_error(&body);
            return Err(Error::Api {
                status: status.as_u16(),
                code,
                message: if message.is_empty() { truncate(&body, 500) } else { message },
                retry_after_secs: None,
            });
        }

        let mut acc = StreamAccumulator::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut got_event = false;
        let mut bytes_stream = resp.bytes_stream();
        while let Some(chunk) = bytes_stream.next().await {
            buf.extend_from_slice(&chunk?);
            // 逐「整行」处理 (未完成的尾行留在 buf, 避免在多字节边界切坏)。
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                // 跳过空行 与以 ':' 开头的 keep-alive 注释 (§8)。
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    return if got_event {
                        Ok(acc.into_response())
                    } else {
                        Err(Error::Stream("stream ended without events".to_string()))
                    };
                }
                match serde_json::from_str::<ChatStreamChunk>(payload) {
                    Ok(c) => {
                        on_chunk(&c); // 先回调 (逐字流式), 再并入累积器
                        acc.push(c);
                        got_event = true;
                    }
                    Err(e) => return Err(Error::Stream(format!("bad SSE chunk: {e}"))),
                }
            }
        }
        // 流结束但无显式 [DONE]: 有事件就用累积结果, 否则报错 (触发上层 fallback)。
        if got_event {
            Ok(acc.into_response())
        } else {
            Err(Error::Stream("empty stream".to_string()))
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::wire::{ChatRequest, Message};

    #[tokio::test]
    #[ignore = "hits real DeepSeek; run with --ignored and DEEPSEEK_API_KEY set"]
    async fn live_stream_returns_text() {
        let client = DeepSeekClient::new(DeepSeekConfig::from_env().unwrap()).unwrap();
        let req = ChatRequest::new(MODEL, vec![Message::user("Say hi in three words.")]);
        let resp = client.chat_stream(&req).await.unwrap();
        let content = resp.choices[0].message.content.clone().unwrap_or_default();
        assert!(!content.trim().is_empty(), "got empty content: {content:?}");
        eprintln!("stream content> {content}");
    }
}

/// 解析 OpenAI 兼容错误体 `{"error":{"code","message",...}}` (§16)。
fn parse_api_error(body: &str) -> (Option<String>, String) {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .map(|e| {
            let code = e.get("code").and_then(Value::as_str).map(str::to_string);
            let msg = e.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            (code, msg)
        })
        .unwrap_or((None, String::new()))
}

/// 退避 jitter 的伪随机源 (无需 `rand` 依赖): 取系统时间纳秒低位映射到 [0,1)。
fn pseudo_jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 1000.0
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
