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
