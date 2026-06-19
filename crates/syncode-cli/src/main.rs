//! SynCode CLI — headless 装配入口 + 一个真实的 agentic 任务演示 (写代码 → 编译运行 → 自验)。

use std::sync::Arc;
use syncode_core::{AgentLoop, Session, ToolRegistry};
use syncode_llm::wire::Role;
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_tools::register_builtins;

#[tokio::main]
async fn main() {
    // 草稿目录: 让模型在里面写一个 Rust 程序、用 Bash 编译运行、自验输出。
    let dir = std::env::temp_dir().join("syncode_bash_demo");
    let _ = std::fs::create_dir_all(&dir);
    for f in ["sum.rs", "sum.exe", "sum.pdb"] {
        let _ = std::fs::remove_file(dir.join(f)); // 清上次产物
    }
    let dir_str = dir.to_string_lossy().to_string();
    println!("scratch dir: {dir_str}");

    let cfg = match DeepSeekConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            println!("DEEPSEEK_API_KEY not set — skipping live agentic demo (skeleton still OK).");
            return;
        }
    };
    let client = DeepSeekClient::new(cfg).expect("client");

    // 挂全部内置工具 (Read/Write/Edit/Grep/AstGrep/AstEdit/Lsp/Bash)。
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry);
    println!("tools: {:?}", registry.names());

    let mut agent = AgentLoop::new(Arc::new(client), registry);
    let mut session = Session::with_system(
        "You are SynCode, a coding agent with file tools and a Bash tool. Use absolute paths. \
         Verify your work by actually running it. Be concise.",
    );
    session.push_user(format!(
        "In the directory {dir_str}, write a Rust program `sum.rs` that prints the sum of the \
         integers 1 to 10 inclusive. Then use the Bash tool to compile it with rustc and run the \
         resulting executable. Report the exact number it printed."
    ));

    println!("\nrunning agentic turn against deepseek-v4-pro …\n");
    match agent.run_turn(&mut session).await {
        Ok(()) => {
            // 工具调用序列 (看模型实际怎么用我们的工具)。
            let mut tool_seq = Vec::new();
            for m in session.messages() {
                if m.role == Role::Assistant {
                    if let Some(tcs) = &m.tool_calls {
                        for tc in tcs {
                            tool_seq.push(tc.function.name.clone());
                        }
                    }
                }
            }
            println!("tool-call sequence: {tool_seq:?}");

            if let Some(reply) = session.messages().iter().rev().find(|m| {
                m.role == Role::Assistant && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
            }) {
                println!("\nassistant> {}", reply.content.as_deref().unwrap_or(""));
            }

            let wrote = dir.join("sum.rs").exists();
            let used_bash = tool_seq.iter().any(|n| n == "Bash");
            let saw_55 = session
                .messages()
                .iter()
                .any(|m| m.content.as_deref().is_some_and(|c| c.contains("55")));
            println!(
                "\nsum.rs written: {wrote} | used Bash: {used_bash} | result 55 observed: {saw_55} \
                 — loop closed: {}",
                if wrote && used_bash && saw_55 { "YES ✓" } else { "no" }
            );
        }
        Err(e) => println!("turn failed: {e}"),
    }
}
