//! Wire 类型: DeepSeek (OpenAI 兼容) `/chat/completions` 的请求/响应模型。
//!
//! 我们自己持有这些类型, 以便对序列化做精确控制 —— 尤其是
//! `Message::reasoning_content` 的三态 (见下), 这是 context 裁切的核心机制。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `skip_serializing_if` 用的 bool helper (避免 `Not::not` 的引用 impl 歧义)。
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// 消息角色 (§6.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

/// 一条对话消息。
///
/// `reasoning_content` 三态 (指南 §7.4/§7.5, 2026-06-16 实测) —— context 裁切的核心:
/// - `Some(cot)`   → 回传完整思维链。**保留 = 模型会读到并用于推理** (实测: closed 工具轮的
///                   CoT 仍被喂给模型)。用于需要推理连续性的最近若干工具轮。
/// - `Some("")`    → 空串 `"reasoning_content":""`。满足校验但**丢掉该轮 CoT 文字** (回收 token)。
/// - `None`        → 整字段省略。仅「已被后续 user 关闭的历史工具轮」可这样 (仍 200), 但这会
///                   **丢掉该轮推理连续性, 不是免费** —— 故只对超出"保留窗口"的旧轮用。
///
/// 400 边界 (精确): 在途跨度内「第 2 个及以后」的 tool_calls 轮缺 reasoning_content 才 400;
/// 「第一个 (或唯一) 工具轮」省略也不报错 (首轮豁免)。统一用 `Some("")` 最稳, 不依赖该豁免。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    /// `role == Tool` 时, 指回对应的 `tool_calls[].id` (§11.1)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: Some(content.into()), ..Default::default() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: Some(content.into()), ..Default::default() }
    }
    /// `role == Tool` 的结果消息 (§11.1)。
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }
    /// 该轮是否携带 tool_calls (决定 §7.4/§7.5 的 reasoning_content 回传义务)。
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
    }
}

/// 模型发起的一次函数调用 (§11)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

fn default_function_kind() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// `arguments` 是 **JSON 字符串** (非对象), 需二次解析 (§11.1)。
    pub arguments: String,
}

/// 工具定义 (请求侧 `tools[]`, §11)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 参数 JSON Schema (§11)。strict 模式三要素见指南 §11.4。
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
}

/// 思考模式开关 (§7.1)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub kind: ThinkingType,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingType {
    Enabled,
    Disabled,
}

/// 思考强度 (§7.1)。复杂 agent 场景建议 `Max`。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    High,
    Max,
}

/// `/chat/completions` 请求体 (§6.1)。
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    /// `none` | `auto` | `required` | `{type,function}` (§11.2)。用 Value 容纳全形态。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 思考模式下被静默忽略 (§7.2)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
    /// `{"type":"json_object"}` 开启 JSON 输出 (§10)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    /// `{"include_usage": true}`: 流式下在末块带 usage (§8)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
}

impl ChatRequest {
    /// 以思考模式 + 给定 messages 构造一个基础请求。
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            thinking: Some(Thinking { kind: ThinkingType::Enabled }),
            reasoning_effort: Some(ReasoningEffort::High),
            tools: None,
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            stop: None,
            stream: false,
            response_format: None,
            stream_options: None,
        }
    }
}

/// `/chat/completions` 非流式响应 (§6.2)。
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub index: u32,
    pub message: Message,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
}

/// `finish_reason` 取值 (§6.2)。`InsufficientSystemResource` 是 DeepSeek 特有, 可重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    InsufficientSystemResource,
}

/// 用量字段 (§6.3)。含上下文缓存命中/未命中拆分 (§12)。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompletionTokensDetails {
    /// 思维链 (CoT) 消耗的 token 数 (思考模式)。
    #[serde(default)]
    pub reasoning_tokens: u64,
}
