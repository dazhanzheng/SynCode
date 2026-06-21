//! SynCode CLI — task-driven headless entrypoint.
//!
//! 给一个任务 (argv 或默认), 在当前工作区跑一个 agent turn (内部多轮 tool-call 直到收尾),
//! 然后落 transcript + token 统计。审批器 (PolicyApprover) / 写收容 (FsScope) / 工具 cwd 全用同一个
//! project_root (单一真相)。无 `DEEPSEEK_API_KEY` 则什么都不跑。

use std::sync::Arc;
use syncode_core::permission::PolicyApprover;
use syncode_core::{AgentLoop, FsScope, Session, ToolRegistry};
use syncode_llm::context::estimate_tokens;
use syncode_llm::wire::{Message, Role};
use syncode_llm::{DeepSeekClient, DeepSeekConfig};
use syncode_tools::register_builtins;

/// 默认压栈任务: 多文件、需用代码智能定位 + 编译闭环 —— 用来压真实多轮 agent 行为。
const DEFAULT_TASK: &str = "Add a default trait method `fn is_readonly(&self) -> bool { false }` to \
    the `Tool` trait (in crates/syncode-core/src/tool.rs). Then override it to return `true` in exactly \
    the read-only tools — the ones that do not modify files or run commands: Read, Grep, AstGrep, and \
    Lsp. Use the code-intelligence tools (Lsp / AstGrep / Grep) to find the Tool trait definition and \
    its implementors before editing. After editing, build with `cargo build -p syncode-core -p \
    syncode-tools` via the Bash tool and fix any compile errors. Report exactly which files you changed.";

#[tokio::main]
async fn main() {
    let cfg = match DeepSeekConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            println!("DEEPSEEK_API_KEY not set — nothing to run.");
            return;
        }
    };
    let client = DeepSeekClient::new(cfg).expect("client");

    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry);

    let project_root = std::env::current_dir().unwrap_or_default();
    println!("workspace: {}", project_root.display());
    println!("tools: {:?}", registry.names());

    let mut agent = AgentLoop::new(Arc::new(client), registry)
        .with_approver(Arc::new(PolicyApprover::new(&project_root)))
        .with_fs_scope(Some(Arc::new(FsScope::new(&project_root))))
        .with_cwd(&project_root)
        .with_sub_agents(true); // 顶层启用子 agent 派生 (子 agent 内部恒禁用, 深度 1)

    let task: String = {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.is_empty() { DEFAULT_TASK.to_string() } else { args.join(" ") }
    };

    let mut session = Session::with_system(syncode_core::system_prompt(&project_root));
    session.push_user(&task);

    println!("\n=== task ===\n{task}\n\n=== running against deepseek (one turn, multi tool-call) ===\n");
    let result = agent.run_turn(&mut session).await;

    let msgs = session.messages();
    let tokens = estimate_tokens(msgs);
    let tool_seq: Vec<String> = msgs
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.function.name.clone())
        .collect();

    let transcript = render_transcript(msgs);
    let log_path = std::env::temp_dir().join("syncode_run_transcript.txt");
    let _ = std::fs::write(&log_path, &transcript);

    match result {
        Ok(()) => {
            if let Some(reply) = msgs.iter().rev().find(|m| {
                m.role == Role::Assistant && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
            }) {
                println!("\nassistant> {}", reply.content.as_deref().unwrap_or(""));
            }
        }
        Err(e) => println!("\nturn failed: {e}"),
    }

    println!("\n--- run summary ---");
    println!("messages: {} | est tokens: {tokens} | tool calls: {}", msgs.len(), tool_seq.len());
    println!("tool-call sequence: {tool_seq:?}");
    println!("transcript written: {}", log_path.display());
}

/// 把 canonical 消息日志渲染成人读 transcript (供事后 review + 喂压缩评估)。
fn render_transcript(msgs: &[Message]) -> String {
    let mut s = String::new();
    for (i, m) in msgs.iter().enumerate() {
        s.push_str(&format!("\n[{i}] {:?}\n", m.role));
        if let Some(r) = &m.reasoning_content {
            if !r.is_empty() {
                s.push_str(&format!("  (reasoning: {} chars)\n", r.len()));
            }
        }
        if let Some(c) = &m.content {
            if !c.trim().is_empty() {
                s.push_str(&format!("  {}\n", c.replace('\n', "\n  ")));
            }
        }
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                s.push_str(&format!("  -> {}({})\n", tc.function.name, tc.function.arguments));
            }
        }
        if m.role == Role::Tool {
            if let Some(c) = &m.content {
                let preview: String = c.chars().take(400).collect();
                s.push_str(&format!("  result: {}\n", preview.replace('\n', "\n  ")));
            }
        }
    }
    s
}
