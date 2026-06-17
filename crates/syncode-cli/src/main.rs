//! SynCode CLI — headless 装配入口 + 一个真实的 agentic 文件任务演示。

use std::sync::Arc;
use syncode_core::{AgentLoop, Session, ToolRegistry};
use syncode_llm::wire::Role;
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_tools::{EditTool, GrepTool, ReadTool, WriteTool};

#[tokio::main]
async fn main() {
    // 草稿文件 (含一个拼写错误, 让模型去 Read → Edit 修)。
    let path = std::env::temp_dir().join("syncode_demo.txt");
    let original = "The quikc brown fox jumps over the lazy dog.\n";
    std::fs::write(&path, original).expect("write scratch file");
    let path_str = path.to_string_lossy().to_string();
    println!("scratch: {path_str}");
    println!("  before: {}", original.trim());

    let cfg = match DeepSeekConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            println!("DEEPSEEK_API_KEY not set — skipping live agentic demo (skeleton still OK).");
            return;
        }
    };
    let client = DeepSeekClient::new(cfg).expect("client");

    // 只挂文件工具 (Bash 仍 todo!(), 不注册以免被调到)。
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool));
    registry.register(Arc::new(WriteTool));
    registry.register(Arc::new(EditTool));
    registry.register(Arc::new(GrepTool));
    println!("tools: {:?}", registry.names());

    let mut agent = AgentLoop::new(Arc::new(client), registry);
    let mut session =
        Session::with_system("You are SynCode, a coding agent. Use the provided tools to do file tasks. Be concise.");
    session.push_user(format!(
        "The file at {path_str} has a typo: the word 'quikc' should be 'quick'. \
         Read the file, fix it with the Edit tool, then confirm in one sentence."
    ));

    println!("\nrunning agentic turn against deepseek-v4-pro …\n");
    match agent.run_turn(&mut session).await {
        Ok(()) => {
            let after = std::fs::read_to_string(&path).unwrap_or_default();
            println!("  after:  {}", after.trim());

            let tool_results = session.messages().iter().filter(|m| m.role == Role::Tool).count();
            let assistant_turns = session.messages().iter().filter(|m| m.role == Role::Assistant).count();
            println!(
                "\nturn ok — {} assistant turns, {} tool results, {} total messages.",
                assistant_turns,
                tool_results,
                session.messages().len()
            );
            if let Some(reply) = session
                .messages()
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant && m.content.as_deref().is_some_and(|c| !c.trim().is_empty()))
            {
                println!("assistant> {}", reply.content.as_deref().unwrap_or(""));
            }
            println!(
                "\nfix applied: {}",
                if after.contains("quick brown") { "YES ✓" } else { "no" }
            );
        }
        Err(e) => println!("turn failed: {e}"),
    }
}
