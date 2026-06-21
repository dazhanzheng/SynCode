//! 工具注册表。

use crate::tool::Tool;
use std::collections::HashMap;
use std::sync::Arc;
use syncode_llm::wire::ToolDef;

/// 名称 → 工具的注册表。`definitions()` 产出发给模型的工具定义 (顺序稳定以吃缓存 §12)。
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 取一个只含给定名字的工具子集 (子 agent 限权用, 如只读 explore)。未知名字忽略。
    pub fn subset(&self, keep: &[&str]) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for name in keep {
            if let Some(t) = self.tools.get(*name) {
                r.tools.insert(t.name().to_string(), t.clone());
            }
        }
        r
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 排序后的工具名 (稳定顺序)。
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.keys().cloned().collect();
        v.sort();
        v
    }

    /// 发给模型的全部工具定义。按名称排序以保证前缀字节稳定 (§12)。
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.names()
            .into_iter()
            .filter_map(|n| self.tools.get(&n).map(|t| t.to_def()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};
    use async_trait::async_trait;
    use serde_json::{json, Value};

    struct Named(&'static str);
    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "x"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{},"additionalProperties":false})
        }
        async fn call(&self, _a: Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("ok"))
        }
    }

    #[test]
    fn subset_keeps_only_named_tools() {
        let mut reg = ToolRegistry::new();
        for n in ["Read", "Grep", "Write", "Bash"] {
            reg.register(Arc::new(Named(n)));
        }
        // 只读子集 (explore/review 限权): 写/执行类被滤掉。
        let ro = reg.subset(&["Read", "Grep", "Nonexistent"]);
        assert_eq!(ro.names(), vec!["Grep".to_string(), "Read".to_string()]);
        assert!(ro.get("Write").is_none(), "Write must be filtered out of a read-only subset");
        assert!(ro.get("Bash").is_none());
    }
}
