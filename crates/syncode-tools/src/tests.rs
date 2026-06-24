//! 文件工具集成测试: 验证 Read/Write/Edit/Grep 的契约 + 共享缓存联动 (必先读 / stale / 原子写)。

use crate::{
    AstEditTool, AstGrepTool, BashOutputTool, BashTool, EditTool, GlobTool, GrepTool, LspTool,
    ReadTool, WriteTool,
};
use serde_json::json;
use std::sync::Arc;
use syncode_core::tool::{Tool, ToolCtx};
use syncode_core::FileStateCache;

fn ctx() -> ToolCtx {
    ToolCtx::new(Arc::new(FileStateCache::new()), std::env::current_dir().unwrap())
}

/// make_diff 产标准 unified diff: 有 `@@` hunk 头、改动行带 +/- 前缀; 无变化 → None。
/// 这是 UI diff 视图逐行着色所依赖的契约。
#[test]
fn make_diff_produces_unified_hunks() {
    use crate::fsutil::make_diff;
    assert!(make_diff("f.rs", "same\n", "same\n").is_none(), "no change → None");
    let d = make_diff("f.rs", "let a = 1;\nkeep\n", "let a = 2;\nkeep\n").expect("changed → Some");
    assert_eq!(d.path, "f.rs");
    assert!(d.unified.contains("@@"), "has a hunk header: {}", d.unified);
    assert!(
        d.unified.lines().any(|l| l.starts_with('-') && l.contains("= 1")),
        "removed old line present: {}",
        d.unified
    );
    assert!(
        d.unified.lines().any(|l| l.starts_with('+') && l.contains("= 2")),
        "added new line present: {}",
        d.unified
    );
}

/// Edit 工具产出携带 unified diff (供 UI diff 视图), 且不污染回给模型的 content。
#[tokio::test]
async fn edit_attaches_unified_diff() {
    use tempfile::tempdir;
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "let a = 1;\n").unwrap();
    let c = ctx();
    // 必先 Read (建立缓存)。
    ReadTool.call(json!({ "file_path": path.to_str().unwrap() }), &c).await.unwrap();
    let out = EditTool
        .call(
            json!({"file_path": path.to_str().unwrap(), "old_string": "let a = 1;", "new_string": "let a = 2;"}),
            &c,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let d = out.diff.expect("Edit should attach a diff");
    assert!(d.unified.contains("-let a = 1;"), "diff shows removal: {}", d.unified);
    assert!(d.unified.contains("+let a = 2;"), "diff shows addition: {}", d.unified);
    // content (给模型) 不含 diff。
    assert!(!out.content.contains("@@"), "model-facing content must not carry the diff");
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
    // 用 cmd 显式验 env scrub (`%VAR%` 展开语义清晰; scrub 本身与 shell 无关 —— 它在 spawn 前作用于
    // Command 的 env, 默认 shell 现已改 PowerShell, 故这里钉 shell:"cmd" 保持断言成立)。
    // USERNAME 总在父进程 env, 但不在白名单 → 子进程拿不到 → cmd 不展开, 原样回显 %USERNAME%。
    let out = BashTool
        .call(json!({ "command": "echo user=[%USERNAME%]", "shell": "cmd" }), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("%USERNAME%"), "env not scrubbed: {}", out.content);
    // PATH 在白名单 → 被回填 → cmd 能展开 (不原样输出 %PATH%)。
    let out2 = BashTool
        .call(json!({ "command": "echo path=[%PATH%]", "shell": "cmd" }), &ctx)
        .await
        .unwrap();
    assert!(!out2.content.contains("%PATH%"), "PATH should be allowlisted: {}", out2.content);
}

// ---- Bash 执行路径的 Unix/macOS 覆盖 (此前全是 cfg(windows), Darwin 这条路从没自动化测过) ----

#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_kills_the_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("gc.pid");
    let ctx = ctx();
    // sh 后台起一个 sleep 30 (孙进程, 非交互 shell 下与 sh 同进程组), 记录其 pid, 然后 wait 阻塞
    // → 必触发 500ms 超时。容器超时走 killpg(pgid, SIGKILL) 杀整组: 若只杀直接子进程 (sh) 漏掉
    // 孙进程 sleep, 本测试会抓到泄漏 (sleep 仍存活)。
    let cmd = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
    let out = BashTool.call(json!({ "command": cmd, "timeout_ms": 500 }), &ctx).await.unwrap();
    assert!(out.content.contains("timed out"), "should time out: {}", out.content);

    let pid = std::fs::read_to_string(&pidfile).unwrap_or_default().trim().to_string();
    assert!(!pid.is_empty(), "grandchild pid was not recorded");
    // killpg 后孙进程被 SIGKILL 并由 launchd/init 回收 → `kill -0` 失败 (ESRCH)。给点时间轮询。
    let mut alive = true;
    for _ in 0..40 {
        let st = std::process::Command::new("kill").arg("-0").arg(&pid).output().unwrap();
        if !st.status.success() {
            alive = false;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!alive, "grandchild sleep (pid {pid}) survived timeout → process tree not killed");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_scrubs_parent_env_keeps_essentials() {
    let ctx = ctx();
    // cargo 跑测试时在父进程 env 注入 CARGO_MANIFEST_DIR (不在 scrub 白名单) → 子进程必须拿不到。
    // (用它而非 set_var: 多线程下 set_var 在 2024 edition 不安全且会与其它测试竞争。)
    let out = BashTool
        .call(json!({ "command": "printf 'manifest=[%s]' \"$CARGO_MANIFEST_DIR\"" }), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("manifest=[]"), "non-allowlisted env not scrubbed: {}", out.content);
    // PATH 在白名单 → 回填 → 非空, 不应输出 path=[]。
    let out2 = BashTool
        .call(json!({ "command": "printf 'path=[%s]' \"$PATH\"" }), &ctx)
        .await
        .unwrap();
    assert!(!out2.content.contains("path=[]"), "PATH should be allowlisted: {}", out2.content);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "invokes rustc; slow; verifies the scrubbed env can still build Rust on macOS"]
async fn bash_can_compile_and_run_rust_under_scrubbed_env() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hello.rs"),
        "fn main() { let s: i32 = (1..=10).sum(); println!(\"sum={s}\"); }",
    )
    .unwrap();
    let ctx = ToolCtx::new(Arc::new(FileStateCache::new()), dir.path().to_path_buf());
    // 关键: env 被 scrub 后, rustc 还能靠白名单里的 CARGO_HOME/RUSTUP_HOME 找到工具链并编译+运行?
    let out = BashTool
        .call(json!({ "command": "rustc hello.rs -o hello && ./hello", "timeout_ms": 120000 }), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("sum=55"), "build/run under scrubbed env failed:\n{}", out.content);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn bash_seatbelt_confines_writes_to_workspace() {
    // 端到端验证 Seatbelt 接进 Bash 路径 (opt-in "sandbox":true): fork 前编译 profile, 子进程 pre_exec 装载。
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolCtx::new(Arc::new(FileStateCache::new()), dir.path().to_path_buf());

    // cwd 内写 → 放行 (Seatbelt 写根 = cwd + tmpdir)。
    let inside = dir.path().join("ok.txt");
    let out = BashTool
        .call(json!({ "command": format!("echo hi > '{}'", inside.display()), "sandbox": true }), &ctx)
        .await
        .unwrap();
    assert!(inside.exists(), "in-cwd sandboxed write should land: {}", out.content);

    // 写根外 (HOME 下, 不在 cwd/tmpdir) → Seatbelt 内核层拒, 不落盘。
    let Some(home) = std::env::var_os("HOME") else { return };
    let outside = std::path::PathBuf::from(home).join(".syncode_seatbelt_ESCAPE.txt");
    let _ = std::fs::remove_file(&outside);
    let out = BashTool
        .call(json!({ "command": format!("echo evil > '{}'", outside.display()), "sandbox": true }), &ctx)
        .await
        .unwrap();
    let escaped = outside.exists();
    let _ = std::fs::remove_file(&outside);
    assert!(!escaped, "sandboxed out-of-root write must be kernel-denied: {}", out.content);
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "slow; runs cargo build under Seatbelt to verify the default sandbox doesn't break Rust builds"]
async fn bash_sandbox_allows_cargo_build() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"sbx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"sbx\"\npath = \"main.rs\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() { println!(\"ok\"); }").unwrap();
    let ctx = ToolCtx::new(Arc::new(FileStateCache::new()), dir.path().to_path_buf());
    // 沙箱写根含 cwd + ~/.cargo + ~/.rustup, 故 cargo 能写 target/ 与 registry 缓存 / .package-cache 锁。
    // 若写根少了构建缓存, cargo 会在沙箱里失败 —— 本测试正是守这条默认策略。
    let out = BashTool
        .call(json!({ "command": "cargo build 2>&1", "sandbox": true, "timeout_ms": 180000 }), &ctx)
        .await
        .unwrap();
    assert!(
        out.content.contains("Finished") || out.content.contains("Compiling sbx"),
        "cargo build under sandbox failed (build-cache write blocked?):\n{}",
        out.content
    );
}

#[tokio::test]
async fn glob_lists_files_respecting_gitignore_and_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".gitignore"), "ignored.txt\ntarget/\n").unwrap();
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(root.join("notes.txt"), "x\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "secret\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("c.rs"), "fn c() {}\n").unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("target").join("junk.rs"), "fn junk() {}\n").unwrap();

    let ctx = ToolCtx::new(Arc::new(FileStateCache::new()), root.to_path_buf());

    // pattern *.rs → 任意深度的 .rs 命中 (含 src/c.rs); 跳过 gitignore 的 target/; 非 .rs 不列。
    let out = GlobTool.call(json!({ "pattern": "*.rs" }), &ctx).await.unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("a.rs"), "{}", out.content);
    assert!(out.content.contains("c.rs"), "src/c.rs should match recursively: {}", out.content);
    assert!(!out.content.contains("notes.txt"), "non-.rs excluded: {}", out.content);
    assert!(!out.content.contains("junk.rs"), "gitignored target/ excluded: {}", out.content);

    // 无 pattern → 列全部非忽略文件; ignored.txt 被 .gitignore 排除。
    let all = GlobTool.call(json!({}), &ctx).await.unwrap();
    assert!(all.content.contains("notes.txt"), "{}", all.content);
    assert!(!all.content.contains("ignored.txt"), "gitignored file excluded: {}", all.content);
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
