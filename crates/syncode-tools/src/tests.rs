//! 文件工具集成测试: 验证 Read/Write/Edit/Grep 的契约 + 共享缓存联动 (必先读 / stale / 原子写)。

use crate::{AstEditTool, AstGrepTool, EditTool, GrepTool, ReadTool, WriteTool};
use serde_json::json;
use std::sync::Arc;
use syncode_core::tool::{Tool, ToolCtx};
use syncode_core::FileStateCache;

fn ctx() -> ToolCtx {
    ToolCtx::new(Arc::new(FileStateCache::new()), std::env::current_dir().unwrap())
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
