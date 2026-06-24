//! TodoWrite: 让模型把多步任务**外化**成一份耐久任务清单 (借鉴 CC, §5)。
//!
//! 价值: ① 引导模型拆解 / 规划多步任务、不跑偏; ② 计划是**耐久状态** —— 即便对话被裁切, 清单也以
//! 工具结果 (受最近窗口保护) + UI 面板留存, 支撑高自治长任务 (支柱 2)。纯进程内, 零外部依赖, 安全。
//!
//! 语义 = **整表替换** (每次传完整清单, 与 CC 一致): 模型每次发当前完整的 todos, 工具记录 + 回显 + 发
//! `Todos` 事件给 UI。不分类 (安全状态更新)。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{TodoItem, TodoStatus, Tool, ToolCtx, ToolError, ToolOutput};

pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn description(&self) -> &str {
        "Maintain a structured task list for the current work. Use it to plan a multi-step task up \
         front and to track progress as you go — it keeps you organized and shows the user what you \
         are doing.\n\
         When to use: tasks with 3+ distinct steps, or non-trivial work that benefits from a plan. \
         Skip it for a single trivial step.\n\
         How to use: send the COMPLETE list every time (it replaces the previous list). Each item has \
         `content` and `status` (pending | in_progress | completed). Keep exactly ONE item \
         in_progress at a time; mark an item completed the moment it is done (do not batch); add new \
         items as they emerge. Update the list as the first thing you do when starting a step."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete, updated task list (replaces the previous one).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "What the step is." },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Step status."
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let raw = args
            .get("todos")
            .ok_or_else(|| ToolError::InvalidArgs("todos is required".into()))?;
        let todos: Vec<TodoItem> = serde_json::from_value(raw.clone())
            .map_err(|e| ToolError::InvalidArgs(format!("invalid todos: {e}")))?;

        // 轻校验: 至多一个 in_progress (与协议一致, 只提示不阻断)。
        let in_progress = todos.iter().filter(|t| t.status == TodoStatus::InProgress).count();
        let mut note = String::new();
        if in_progress > 1 {
            note = format!(
                " (note: {in_progress} items are in_progress; keep exactly one in_progress at a time)"
            );
        }

        let rendered = render(&todos);
        let msg = format!("Todo list updated ({} items).{note}\n{rendered}", todos.len());
        Ok(ToolOutput::ok(msg).with_todos(todos))
    }
}

/// 渲染成 checklist (回给模型读 + 人看): ☐ pending / ▶ in_progress / ☑ completed。
fn render(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .map(|t| {
            let glyph = match t.status {
                TodoStatus::Pending => "☐",
                TodoStatus::InProgress => "▶",
                TodoStatus::Completed => "☑",
            };
            format!("{glyph} {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use syncode_core::file_state::FileStateCache;

    fn ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(FileStateCache::new()), std::env::temp_dir())
    }

    #[tokio::test]
    async fn writes_and_renders_list() {
        let args = json!({"todos": [
            {"content": "scan the module", "status": "completed"},
            {"content": "write the fix", "status": "in_progress"},
            {"content": "run tests", "status": "pending"}
        ]});
        let out = TodoWriteTool.call(args, &ctx()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        let todos = out.todos.expect("todos payload present");
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[1].status, TodoStatus::InProgress);
        assert!(out.content.contains("☑ scan the module"), "got: {}", out.content);
        assert!(out.content.contains("▶ write the fix"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn flags_multiple_in_progress() {
        let args = json!({"todos": [
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "in_progress"}
        ]});
        let out = TodoWriteTool.call(args, &ctx()).await.unwrap();
        assert!(out.content.contains("keep exactly one in_progress"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn rejects_bad_status() {
        let args = json!({"todos": [{"content": "x", "status": "bogus"}]});
        let err = TodoWriteTool.call(args, &ctx()).await.unwrap_err();
        assert!(format!("{err}").contains("invalid todos"), "got: {err}");
    }
}
