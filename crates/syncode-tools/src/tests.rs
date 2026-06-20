//! 文件工具集成测试: 验证 Read/Write/Edit/Grep 的契约 + 共享缓存联动 (必先读 / stale / 原子写)。

use crate::{
    AstEditTool, AstGrepTool, BashOutputTool, BashTool, EditTool, GrepTool, LspTool, ReadTool,
    WriteTool,
};
use serde_json::json;
use std::sync::Arc;
use syncode_core::tool::{Tool, ToolCtx};
use syncode_core::FileStateCache;

fn ctx() -> ToolCtx {
    ToolCtx::new(Arc::new(FileStateCache::new()), std::env::current_dir().unwrap())
}

/// run_in_background (§5.5): Bash 后台跑 → 立刻拿 task id → BashOutput 轮询读到输出 + 退出状态。
#[tokio::test]
async fn bash_run_in_background_and_poll() {
    let c = ctx(); // 同一个 ctx → 共享后台注册表
    let started = BashTool
        .call(json!({"command": "echo hello-bg", "run_in_background": true}), &c)
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let id = started.content.split('`').nth(1).unwrap_or("").to_string();
    assert!(id.starts_with("bash_"), "unexpected start message: {}", started.content);

    let mut saw_output = false;
    let mut saw_exit = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let out = BashOutputTool.call(json!({ "id": id }), &c).await.unwrap();
        if out.content.contains("hello-bg") {
            saw_output = true;
        }
        if out.content.contains("exited") {
            saw_exit = true;
            break;
        }
    }
    assert!(saw_output, "should have captured background stdout");
    assert!(saw_exit, "task should have reached exited state");

    // 未知 id → 错误。
    let bad = BashOutputTool.call(json!({"id": "bash_nope"}), &c).await.unwrap();
    assert!(bad.is_error);
}

/// 逃逸测试 (P1c): 挂了写收容 (`FsScope`) 后, Write 到授权根**外**必须被构造级拒绝。
#[tokio::test]
async fn write_outside_fs_scope_is_refused() {
    use syncode_core::FsScope;
    let root = tempfile::tempdir().unwrap();
    let mut c = ToolCtx::new(Arc::new(FileStateCache::new()), root.path().to_path_buf());
    c.fs = Some(Arc::new(FsScope::new(root.path())));

    // 根内写 → 放行 (新文件免读)。
    let inside = root.path().join("ok.txt");
    let out = WriteTool
        .call(json!({"file_path": inside.to_str().unwrap(), "content": "hi"}), &c)
        .await
        .unwrap();
    assert!(!out.is_error, "in-root write should succeed: {}", out.content);
    assert!(inside.exists());

    // 根外 (且 temp 外) 写 → 被写收容拒 (temp 本身是写根, 故取 temp 父目录之外)。
    if let Some(above_temp) = std::env::temp_dir().parent() {
        let escaped = above_temp.join("syncode_escape_evil.txt");
        let res = WriteTool
            .call(json!({"file_path": escaped.to_str().unwrap(), "content": "pwned"}), &c)
            .await;
        assert!(res.is_err(), "out-of-root write must be refused");
        assert!(!escaped.exists(), "the escaped file must NOT have been written");
    }
}

#[tokio::test]
async fn edit_requires_prior_read_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "hello world\nsecond line\n").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();

    // 没 Read 就 Edit → 必先读错误。
    let out = EditTool
        .call(json!({"file_path": fp, "old_string": "hello", "new_string": "hi"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("File has not been read yet"), "{}", out.content);

    // Read 一次。
    let r = ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    assert!(r.content.contains("hello world"));

    // Read 后 Edit → 成功。
    let e = EditTool
        .call(json!({"file_path": fp, "old_string": "hello", "new_string": "hi"}), &ctx)
        .await
        .unwrap();
    assert!(!e.is_error && e.content.contains("updated successfully"), "{}", e.content);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hi world\nsecond line\n");
}

#[tokio::test]
async fn edit_stale_after_external_modification() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("s.txt");
    std::fs::write(&f, "v1").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    // 外部改动 (内容变 + mtime 变新)。
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&f, "v2-external").unwrap();
    let out = EditTool
        .call(json!({"file_path": fp, "old_string": "v2", "new_string": "x"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("modified since read"), "{}", out.content);
}

#[tokio::test]
async fn edit_multiple_matches_requires_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("b.txt");
    std::fs::write(&f, "x x x").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();

    let out = EditTool
        .call(json!({"file_path": fp, "old_string": "x", "new_string": "y"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("Found 3 matches"), "{}", out.content);

    let ok = EditTool
        .call(json!({"file_path": fp, "old_string": "x", "new_string": "y", "replace_all": true}), &ctx)
        .await
        .unwrap();
    assert!(!ok.is_error);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "y y y");
}

#[tokio::test]
async fn write_new_file_no_read_needed_existing_needs_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx();

    // 新文件: 免读。
    let nf = dir.path().join("new.txt");
    let out = WriteTool
        .call(json!({"file_path": nf.to_str().unwrap(), "content": "abc"}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error && out.content.contains("created successfully"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&nf).unwrap(), "abc");

    // 外部已存在文件、没 Read → 必先读错误。
    let ef = dir.path().join("ext.txt");
    std::fs::write(&ef, "old").unwrap();
    let out2 = WriteTool
        .call(json!({"file_path": ef.to_str().unwrap(), "content": "new"}), &ctx)
        .await
        .unwrap();
    assert!(out2.is_error && out2.content.contains("File has not been read yet"), "{}", out2.content);
}

#[tokio::test]
async fn write_then_rewrite_via_own_cache() {
    // Write 写完会回写缓存, 故对同一路径再次 Write 能过「必先读」门。
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("w.txt");
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    WriteTool.call(json!({"file_path": fp, "content": "v1"}), &ctx).await.unwrap();
    let out = WriteTool.call(json!({"file_path": fp, "content": "v2"}), &ctx).await.unwrap();
    assert!(!out.is_error && out.content.contains("updated successfully"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "v2");
}

#[tokio::test]
async fn grep_content_and_glob() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "nothing here\n").unwrap();
    let ctx = ctx();
    let root = dir.path().to_str().unwrap();

    let out = GrepTool
        .call(json!({"pattern": "fn \\w+", "path": root, "output_mode": "content"}), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("foo") && out.content.contains("bar"), "{}", out.content);

    let out2 = GrepTool
        .call(json!({"pattern": "fn", "path": root, "glob": "*.rs", "output_mode": "files_with_matches"}), &ctx)
        .await
        .unwrap();
    assert!(out2.content.contains("a.rs") && !out2.content.contains("b.txt"), "{}", out2.content);
}

#[tokio::test]
async fn edit_preserves_crlf_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("crlf.txt");
    std::fs::write(&f, "alpha\r\nbeta\r\n").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    EditTool
        .call(json!({"file_path": fp, "old_string": "alpha", "new_string": "ALPHA"}), &ctx)
        .await
        .unwrap();
    // CRLF 保留, 不被改成 LF。
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "ALPHA\r\nbeta\r\n");
}

// ---- AST 层工具 (syncode-ast 驱动): 结构化搜索 / 结构化改写 / Edit 改后语法护栏 ----

const RUST_FILE: &str = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";

#[tokio::test]
async fn ast_grep_finds_structural_matches_in_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, RUST_FILE).unwrap();
    let ctx = ctx();
    // 单文件: 语言按 .rs 扩展名推断, 无需 lang。
    let out = AstGrepTool
        .call(json!({"pattern": "let $N = $V;", "path": f.to_str().unwrap()}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("let x = 1;") && out.content.contains("let y = 2;"), "{}", out.content);
    // 含行号。
    assert!(out.content.contains(":2:") && out.content.contains(":3:"), "{}", out.content);
}

#[tokio::test]
async fn ast_grep_directory_search_filters_by_lang() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), RUST_FILE).unwrap();
    std::fs::write(dir.path().join("b.txt"), "let z = 9; (not rust)\n").unwrap();
    let ctx = ctx();
    let out = AstGrepTool
        .call(json!({"pattern": "let $N = $V;", "path": dir.path().to_str().unwrap(), "lang": "rust"}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    // 只搜到 .rs 里的, .txt 被 file_types 过滤掉。
    assert!(out.content.contains("a.rs"), "{}", out.content);
    assert!(!out.content.contains("b.txt"), "{}", out.content);
}

#[tokio::test]
async fn ast_grep_bad_pattern_reports_error() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, RUST_FILE).unwrap();
    let ctx = ctx();
    let out = AstGrepTool
        .call(json!({"pattern": "", "path": f.to_str().unwrap()}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("Invalid ast-grep pattern"), "{}", out.content);
}

#[tokio::test]
async fn ast_edit_requires_prior_read() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, RUST_FILE).unwrap();
    let ctx = ctx();
    let out = AstEditTool
        .call(
            json!({"file_path": f.to_str().unwrap(), "pattern": "let $N = $V;", "rewrite": "const $N: i32 = $V;"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("File has not been read yet"), "{}", out.content);
}

#[tokio::test]
async fn ast_edit_rewrites_every_match_and_verifies_syntax() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, RUST_FILE).unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    let out = AstEditTool
        .call(json!({"file_path": fp, "pattern": "let $N = $V;", "rewrite": "const $N: i32 = $V;"}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error && out.content.contains("2 structural replacements"), "{}", out.content);
    let after = std::fs::read_to_string(&f).unwrap();
    assert!(after.contains("const x: i32 = 1;") && after.contains("const y: i32 = 2;"), "{after}");
}

#[tokio::test]
async fn ast_edit_rejects_syntax_breaking_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, RUST_FILE).unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    let out = AstEditTool
        .call(json!({"file_path": fp, "pattern": "let $N = $V;", "rewrite": "let $N = ;"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("Rewrite rejected"), "{}", out.content);
    // 被拒后文件原样不动。
    assert_eq!(std::fs::read_to_string(&f).unwrap(), RUST_FILE);
}

#[tokio::test]
async fn ast_edit_no_match_makes_no_change() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, RUST_FILE).unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    let out = AstEditTool
        .call(json!({"file_path": fp, "pattern": "while $C {}", "rewrite": "loop {}"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("matched nothing"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), RUST_FILE);
}

#[tokio::test]
async fn edit_warns_when_it_breaks_syntax_but_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    std::fs::write(&f, "fn a() {}\n").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    // 把 `{}` 改成 `{` —— 破语法, 但 Edit 仍应用 + 警告 (非阻断)。
    let out = EditTool
        .call(json!({"file_path": fp, "old_string": "{}", "new_string": "{"}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("updated successfully"), "{}", out.content);
    assert!(out.content.contains("syntax error"), "warning missing: {}", out.content);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "fn a() {\n");
}

#[tokio::test]
async fn read_returns_dedup_stub_on_unchanged_reread() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("d.txt");
    std::fs::write(&f, "content here").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();
    let first = ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    assert!(first.content.contains("content here"));
    let second = ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    assert!(second.content.contains("File unchanged since last read"), "{}", second.content);
}

#[tokio::test]
#[ignore = "spawns rust-analyzer; slow and requires ra installed"]
async fn lsp_documentsymbol_via_tool() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("lib.rs");
    std::fs::write(&f, "pub fn alpha() {}\npub struct Beta;\n").unwrap();
    // 工程根 = tempdir (documentSymbol 是语法级, 不需 cargo 工程)。
    let ctx = ToolCtx::new(Arc::new(FileStateCache::new()), dir.path().to_path_buf());
    let out = LspTool
        .call(json!({"operation": "documentSymbol", "file_path": f.to_str().unwrap()}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("alpha") && out.content.contains("Beta"), "{}", out.content);
}

// ---- Bash (跑命令 + 进程容器) ----

#[tokio::test]
async fn bash_runs_command_and_captures_output() {
    let ctx = ctx();
    let out = BashTool
        .call(json!({ "command": "echo hello-from-bash" }), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("hello-from-bash"), "{}", out.content);
}

#[tokio::test]
async fn bash_reports_nonzero_exit_code() {
    let ctx = ctx();
    let out = BashTool.call(json!({ "command": "exit 3" }), &ctx).await.unwrap();
    assert!(out.content.contains("exit code 3"), "{}", out.content);
}

#[cfg(windows)]
#[tokio::test]
async fn bash_timeout_kills_the_process_tree() {
    let ctx = ctx();
    // ping -n 5 ≈ 4s; timeout 500ms 必触发 → 容器杀整树。
    let out = BashTool
        .call(json!({ "command": "ping -n 5 127.0.0.1 >nul", "timeout_ms": 500 }), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("timed out"), "{}", out.content);
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "invokes rustc; slow; verifies the scrubbed env can still build Rust"]
async fn bash_can_compile_and_run_rust_under_scrubbed_env() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hello.rs"),
        "fn main() { let s: i32 = (1..=10).sum(); println!(\"sum={s}\"); }",
    )
    .unwrap();
    let ctx = ToolCtx::new(Arc::new(FileStateCache::new()), dir.path().to_path_buf());
    // 关键: env 被 scrub 后, rustc 还能不能找到 MSVC linker 并编译+运行?
    let out = BashTool
        .call(
            json!({ "command": "rustc hello.rs -o hello.exe && hello.exe", "timeout_ms": 120000 }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.content.contains("sum=55"), "build/run under scrubbed env failed:\n{}", out.content);
}

#[cfg(windows)]
#[tokio::test]
async fn bash_scrubs_parent_env_keeps_essentials() {
    let ctx = ctx();
    // USERNAME 总在父进程 env, 但不在白名单 → 子进程拿不到 → cmd 不展开, 原样回显 %USERNAME%。
    let out = BashTool
        .call(json!({ "command": "echo user=[%USERNAME%]" }), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("%USERNAME%"), "env not scrubbed: {}", out.content);
    // PATH 在白名单 → 被回填 → cmd 能展开 (不原样输出 %PATH%)。
    let out2 = BashTool
        .call(json!({ "command": "echo path=[%PATH%]" }), &ctx)
        .await
        .unwrap();
    assert!(!out2.content.contains("%PATH%"), "PATH should be allowlisted: {}", out2.content);
}

#[tokio::test]
async fn lsp_rejects_unsupported_extension() {
    // 没有为 .zzz 配置语言服务器 → 明确提示 (确定性, 不依赖本机装没装某 server)。
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.zzz");
    std::fs::write(&f, "whatever\n").unwrap();
    let ctx = ctx();
    let out = LspTool
        .call(json!({"operation": "documentSymbol", "file_path": f.to_str().unwrap()}), &ctx)
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("No language server is configured"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn read_emits_cat_n_line_numbers_offset_aware() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("n.txt");
    std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
    let ctx = ctx();
    let fp = f.to_str().unwrap();

    // 全文: cat -n 行号 (右对齐 + tab + 内容)。
    let full = ReadTool.call(json!({ "file_path": fp }), &ctx).await.unwrap();
    assert!(full.content.contains("1\talpha"), "{}", full.content);
    assert!(full.content.contains("2\tbeta"), "{}", full.content);
    assert!(full.content.contains("3\tgamma"), "{}", full.content);

    // 窗口: 行号是**真实文件行号** (offset-aware), 不是从 1 重新数。
    let win = ReadTool.call(json!({ "file_path": fp, "offset": 2, "limit": 1 }), &ctx).await.unwrap();
    assert!(win.content.contains("2\tbeta"), "{}", win.content);
    assert!(!win.content.contains("alpha") && !win.content.contains("gamma"), "{}", win.content);
}
