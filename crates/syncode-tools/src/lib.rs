//! SynCode 内置工具 (借鉴 Claude Code 工具设计 IP, 用 Rust 重写, 架构 §10)。
//!
//! 当前为 stub: 契约 (名称/描述/JSON Schema) 立起来, `call` 为 `todo!()`。
//! 抬上限的方向 (§4) 在各工具 doc 里标注 (如 Grep 改为进程内 `ignore`/`grep` 库)。
#![allow(dead_code, unused_variables)]

mod ast_edit;
mod ast_grep;
mod bash;
mod bash_output;
mod edit;
mod fsutil;
mod grep;
mod lsp;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use ast_edit::AstEditTool;
pub use ast_grep::AstGrepTool;
pub use bash::BashTool;
pub use bash_output::BashOutputTool;
pub use edit::EditTool;
pub use grep::GrepTool;
pub use lsp::LspTool;
pub use read::ReadTool;
pub use write::WriteTool;

use std::sync::Arc;
use syncode_core::ToolRegistry;

/// 注册全部内置工具到 registry。
pub fn register_builtins(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ReadTool));
    registry.register(Arc::new(WriteTool));
    registry.register(Arc::new(EditTool));
    registry.register(Arc::new(GrepTool));
    registry.register(Arc::new(AstGrepTool));
    registry.register(Arc::new(AstEditTool));
    registry.register(Arc::new(LspTool));
    registry.register(Arc::new(BashTool));
    registry.register(Arc::new(BashOutputTool));
}
