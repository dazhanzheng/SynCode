//! DeepSeek HTTP client (§3, §6)。骨架: 装配/配置就绪, 收发逻辑为 `todo!()`。

use crate::error::{Error, Result};
use crate::wire::{ChatRequest, ChatResponse};

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
            message: "DEEPSEEK_API_KEY 环境变量未设置".to_string(),
        })?;
        Ok(Self::new(api_key))
    }
}

/// DeepSeek 对话补全 client。
pub struct DeepSeekClient {
    config: DeepSeekConfig,
    http: reqwest::Client,
}

impl DeepSeekClient {
    pub fn new(config: DeepSeekConfig) -> Result<Self> {
        let http = reqwest::Client::builder().build()?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &DeepSeekConfig {
        &self.config
    }

    /// 非流式对话补全 (§6)。
    ///
    /// TODO: POST `{base_url}/chat/completions`, Bearer 鉴权, 解析 `ChatResponse`;
    /// 容忍 keep-alive 空行 (§8); 对 429/500/503/insufficient_system_resource 退避重试 (§16)。
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        todo!("POST /chat/completions and parse ChatResponse (guide §6, §16)")
    }

    /// 流式对话补全 (§8)。
    ///
    /// TODO: SSE 解析, 必须跳过空行与以 `:` 开头的注释行 (`: keep-alive`);
    /// 读超时按 10 分钟上限设。
    pub async fn chat_stream(&self, request: &ChatRequest) -> Result<ChatResponse> {
        todo!("SSE streaming with keep-alive tolerance (guide §8)")
    }
}
