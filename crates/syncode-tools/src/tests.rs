//! 文件工具集成测试: 验证 Read/Write/Edit/Grep 的契约 + 共享缓存联动 (必先读 / stale / 原子写)。

use crate::{EditTool, GrepTool, ReadTool, WriteTool};
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
