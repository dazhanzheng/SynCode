//! 流式 (SSE) 响应的分片重组 (§8 / §11.5)。
//!
//! DeepSeek 流式下 `tool_calls` 按 `index` 分片到达: 每个工具的「头块」带 `id`/`name` + 空 `arguments`,
//! 之后「续块」只有 `index` + `arguments` 碎片, 需按 index 拼接。本模块把一串 delta chunk 累积成
//! 与非流式等价的 [`ChatResponse`]。

use crate::wire::{ChatResponse, Choice, FinishReason, FunctionCall, Message, Role, ToolCall, Usage};
use serde::Deserialize;

/// 一个 SSE `data:` 块 (`object == "chat.completion.chunk"`)。
#[derive(Debug, Clone, Deserialize)]
pub struct ChatStreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: StreamDelta,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Default)]
struct AccTool {
    id: String,
    name: String,
    arguments: String,
}

/// 累积 SSE delta 分片, 重组成与非流式等价的 [`ChatResponse`] (§11.5)。
#[derive(Default)]
pub struct StreamAccumulator {
    content: String,
    reasoning: String,
    tool_calls: Vec<AccTool>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 吃一个 delta 块: content/reasoning 拼接; tool_calls 按 `index` 缝合
    /// (头块给 `id`/`name`, 续块拼 `arguments`)。
    pub fn push(&mut self, chunk: ChatStreamChunk) {
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }
        for sc in chunk.choices {
            if sc.finish_reason.is_some() {
                self.finish_reason = sc.finish_reason;
            }
            if let Some(c) = sc.delta.content {
                self.content.push_str(&c);
            }
            if let Some(r) = sc.delta.reasoning_content {
                self.reasoning.push_str(&r);
            }
            for frag in sc.delta.tool_calls.into_iter().flatten() {
                let i = frag.index;
                while self.tool_calls.len() <= i {
                    self.tool_calls.push(AccTool::default());
                }
                let slot = &mut self.tool_calls[i];
                if let Some(id) = frag.id {
                    slot.id = id;
                }
                if let Some(f) = frag.function {
                    if let Some(n) = f.name {
                        slot.name = n;
                    }
                    if let Some(a) = f.arguments {
                        slot.arguments.push_str(&a);
                    }
                }
            }
        }
    }

    /// 收尾: 重组成与非流式等价的 [`ChatResponse`]。
    pub fn into_response(self) -> ChatResponse {
        let tool_calls = if self.tool_calls.is_empty() {
            None
        } else {
            Some(
                self.tool_calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| ToolCall {
                        id: if t.id.is_empty() { format!("call_{i}") } else { t.id },
                        kind: "function".to_string(),
                        function: FunctionCall { name: t.name, arguments: t.arguments },
                    })
                    .collect(),
            )
        };
        let message = Message {
            role: Role::Assistant,
            content: Some(self.content),
            reasoning_content: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
            tool_calls,
            tool_call_id: None,
            name: None,
        };
        ChatResponse {
            id: String::new(),
            model: String::new(),
            choices: vec![Choice { index: 0, message, finish_reason: self.finish_reason }],
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Option<String> {
        Some(x.to_string())
    }
    fn delta(
        content: Option<&str>,
        reasoning: Option<&str>,
        tcs: Vec<ToolCallDelta>,
        finish: Option<FinishReason>,
    ) -> ChatStreamChunk {
        ChatStreamChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: content.map(str::to_string),
                    reasoning_content: reasoning.map(str::to_string),
                    tool_calls: if tcs.is_empty() { None } else { Some(tcs) },
                },
                finish_reason: finish,
            }],
            usage: None,
        }
    }
    fn tcd(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(str::to_string),
            function: Some(FunctionDelta { name: name.map(str::to_string), arguments: args.map(str::to_string) }),
        }
    }

    #[test]
    fn reassembles_content_fragments() {
        let mut acc = StreamAccumulator::new();
        acc.push(delta(s("Hello").as_deref(), None, vec![], None));
        acc.push(delta(s(", ").as_deref(), None, vec![], None));
        acc.push(delta(s("world!").as_deref(), None, vec![], Some(FinishReason::Stop)));
        let resp = acc.into_response();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello, world!"));
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
        assert!(resp.choices[0].message.tool_calls.is_none());
    }

    #[test]
    fn reassembles_tool_call_from_index_fragments() {
        let mut acc = StreamAccumulator::new();
        // 头块: id + name + 空 arguments
        acc.push(delta(None, None, vec![tcd(0, Some("c1"), Some("get_weather"), Some(""))], None));
        // 续块: 只有 index + arguments 碎片
        acc.push(delta(None, None, vec![tcd(0, None, None, Some("{\"location\":"))], None));
        acc.push(delta(None, None, vec![tcd(0, None, None, Some(" \"HZ\"}"))], None));
        acc.push(delta(None, None, vec![], Some(FinishReason::ToolCalls)));
        let resp = acc.into_response();
        let tc = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id, "c1");
        assert_eq!(tc.function.name, "get_weather");
        assert_eq!(tc.function.arguments, "{\"location\": \"HZ\"}");
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::ToolCalls));
    }

    #[test]
    fn reassembles_two_parallel_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.push(delta(None, None, vec![tcd(0, Some("c0"), Some("get_weather"), Some("{\"l\":\"a\"}"))], None));
        acc.push(delta(None, None, vec![tcd(1, Some("c1"), Some("get_weather"), Some("{\"l\":\"b\"}"))], None));
        acc.push(delta(None, None, vec![], Some(FinishReason::ToolCalls)));
        let calls = acc.into_response().choices[0].message.tool_calls.clone().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "c0");
        assert_eq!(calls[1].id, "c1");
    }

    #[test]
    fn deserializes_real_captured_chunk() {
        // 取自真实抓包: tool_calls 头块。
        let line = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_00_X","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}],"usage":null}"#;
        let chunk: ChatStreamChunk = serde_json::from_str(line).unwrap();
        let frag = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(frag.id.as_deref(), Some("call_00_X"));
        assert_eq!(frag.function.as_ref().unwrap().name.as_deref(), Some("get_weather"));
    }
}
