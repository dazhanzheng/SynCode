//! SynCode CLI — headless 装配入口。
//!
//! 装配核心组件; 若 `DEEPSEEK_API_KEY` 在环境中, 跑一个真实 turn 作冒烟验证。

use std::sync::Arc;
use syncode_core::{AgentLoop, Session, ToolRegistry};
use syncode_llm::wire::Role;
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_sandbox::{NoopSandbox, Sandbox};

#[tokio::main]
async fn main() {
    println!("SynCode — wiring core components.");

    // 1. 沙箱底座 (占位)。
    let sandbox = NoopSandbox;
    println!("sandbox backend: {}", sandbox.name());

    // 2. 工具 registry + 内置工具 (展示注册; 内置工具 call 仍为 todo!())。
    let mut registry = ToolRegistry::new();
    syncode_tools::register_builtins(&mut registry);
    println!("registered {} tools: {:?}", registry.len(), registry.names());

    // 3. DeepSeek client (从环境装配)。
    let cfg = match DeepSeekConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            println!("DEEPSEEK_API_KEY not set — skipping live turn (skeleton still OK).");
            return;
        }
    };
    println!("deepseek config: base_url={} model={}", cfg.base_url, cfg.model);
    let client = match DeepSeekClient::new(cfg) {
        Ok(c) => c,
        Err(e) => {
            println!("client init failed: {e}");
            return;
        }
    };

    // 4. 冒烟: 用「空工具」registry 跑一个 turn —— 模型只出文本, 不触发内置工具的 todo!()。
    let mut agent = AgentLoop::new(Arc::new(client), ToolRegistry::new());
    let mut session = Session::with_system("You are SynCode. Answer in one short sentence.");
    session.push_user("Reply with a short friendly greeting.");

    println!("\nrunning one live turn against deepseek-v4-pro …");
    match agent.run_turn(&mut session).await {
        Ok(()) => {
            if let Some(reply) = session
                .messages()
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
            {
                println!("assistant> {}", reply.content.as_deref().unwrap_or("(no content)"));
            }
            println!("turn ok; session now has {} messages.", session.messages().len());
        }
        Err(e) => println!("turn failed: {e}"),
    }
    println!("done.");
}
