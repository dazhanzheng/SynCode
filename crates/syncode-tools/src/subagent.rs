//! Task: 把一个**聚焦子任务**委派给子 agent (§5 编排, 支柱 2)。子 agent 在**嵌套 agent loop** 里
//! 自主跑完, 只把**最终结果**返回 —— 中间的多轮工具调用不进编排者 context (这正是 sub-agent 的价值:
//! 大范围检索 / 多步子任务的过程噪音留在子 agent 里, 编排者只拿结论)。
//!
//! 安全: 子 agent 继承父的审批器与 cap-std 写收容 (权限**不放宽**), 且**不能再派子 agent** (深度 1)。
//! 故本工具自身不分类 (`classify=None`): 子 agent 的每个实际动作仍各自过审批闸。派生器由 `AgentLoop`
//! 在启用 sub-agents 时注入 `ToolCtx`; 未启用 (含子 agent 内部) → 返回 model-readable 错误。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{SubAgentRequest, Tool, ToolCtx, ToolError, ToolOutput};

pub struct SubAgentTool;

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "Task"
    }

    fn description(&self) -> &str {
        "Delegate a focused sub-task to a sub-agent and get back only its final result. Use this to \
         keep your own context clean: for a broad search across many files, or a self-contained \
         multi-step subtask whose intermediate tool calls you don't need to see, hand it off here \
         and you receive just the conclusion.\n\
         - description: a short label for the sub-task.\n\
         - prompt: the full, self-contained instructions for the sub-agent (it does not see this \
         conversation, so include everything it needs).\n\
         The sub-agent has the same tools and the same workspace confinement as you, runs \
         autonomously to completion, and cannot itself spawn further sub-agents. Prefer doing small \
         tasks yourself; reach for Task when delegating genuinely saves context or parallelizes work."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "Short label for the sub-task." },
                "prompt": { "type": "string", "description": "Full self-contained instructions for the sub-agent." }
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("description is required".into()))?
            .to_string();
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("prompt is required".into()))?
            .to_string();

        let result = ctx.spawn_sub_agent(SubAgentRequest { description, prompt }).await?;
        Ok(ToolOutput::ok(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use syncode_core::file_state::FileStateCache;
    use syncode_core::tool::SubAgentSpawner;

    fn bare_ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(FileStateCache::new()), std::env::temp_dir())
    }

    #[tokio::test]
    async fn task_without_spawner_errors_gracefully() {
        // ctx.sub_agent = None (未启用 / 子 agent 内部, 深度 1) → model-readable 错误, 不 panic。
        let ctx = bare_ctx();
        let err = SubAgentTool
            .call(json!({"description": "d", "prompt": "p"}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not available"), "got: {err}");
    }

    #[tokio::test]
    async fn task_returns_sub_agent_result() {
        // 注入一个 stub 派生器 (不跑真 LLM) → 验证 工具→派生器→结果 的管线。
        let mut ctx = bare_ctx();
        let spawner: SubAgentSpawner = Arc::new(|req: SubAgentRequest| {
            Box::pin(async move { Ok(format!("sub-agent did: {}", req.description)) })
        });
        ctx.sub_agent = Some(spawner);
        let out = SubAgentTool
            .call(json!({"description": "explore", "prompt": "go"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "sub-agent did: explore");
    }
}
