//! SynCode CLI — headless 装配入口 (骨架)。
//!
//! 当前只装配各核心组件并打印结构, **不接真 API、不跑 loop** (那些是 `todo!()`)。
//! 目的: 证明整套 crate 结构能编译、能 link、能运行。

use std::sync::Arc;
use syncode_core::{AgentLoop, Session, ToolRegistry};
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_sandbox::{NoopSandbox, Sandbox};

fn main() {
    println!("SynCode skeleton — wiring core components (no live API calls).");

    // 1. 沙箱底座 (占位)。
    let sandbox = NoopSandbox;
    println!("sandbox backend: {}", sandbox.name());

    // 2. 工具 registry + 内置工具。
    let mut registry = ToolRegistry::new();
    syncode_tools::register_builtins(&mut registry);
    println!("registered {} tools: {:?}", registry.len(), registry.names());

    // 3. DeepSeek client (从环境装配; 无 key 时仅提示, 不视为失败)。
    match DeepSeekConfig::from_env() {
        Ok(cfg) => {
            println!("deepseek config: base_url={} model={}", cfg.base_url, cfg.model);
            match DeepSeekClient::new(cfg) {
                Ok(client) => {
                    let _agent = AgentLoop::new(Arc::new(client), registry);
                    let _session = Session::with_system("You are SynCode.");
                    println!("agent loop constructed; run_turn() is not implemented yet (todo!).");
                }
                Err(e) => println!("client init failed: {e}"),
            }
        }
        Err(_) => {
            println!("DEEPSEEK_API_KEY not set — skipping client construction (skeleton still OK).");
        }
    }

    println!("done.");
}
