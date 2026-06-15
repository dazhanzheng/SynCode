//! Edit: `old_string` 唯一匹配否则报错 + `replace_all` 开关 + 强约定「改前必须先 Read」(§10)。
//! 抬上限方向: tree-sitter / ast-grep AST 级改写 (语法保证合法) + `ropey` 大文件 (§4)。

use async_trait::async_trait;
use serde_json::{json, Value};
use syncode_core::tool::{Tool, ToolError, ToolOutput};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace an exact, unique occurrence of old_string with new_string in a file. \
         Set replace_all to replace every occurrence. You must Read the file first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file to edit." },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique unless replace_all)." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences instead of requiring uniqueness." }
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    fn is_dangerous(&self) -> bool {
        // 改文件 → 走沙箱授权根 + 审批 (§5.2)。
        true
    }

    async fn call(&self, args: Value) -> Result<ToolOutput, ToolError> {
        todo!("exact unique string replacement / replace_all (guide §10)")
    }
}
