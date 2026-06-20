//! 语言 → 语言服务器映射 (多语言代码智能, 路线图 §5.3)。
//!
//! 按文件扩展名选 server (rust-analyzer / gopls / pyright / typescript-language-server / clangd …),
//! 给出 LSP `languageId`、可执行名 + 参数、以及**工程根标志文件** (向上找哪个 manifest 当根)。
//! **运行时只在本机装了对应 server 时才验得了**; 未装则 `Lsp` 工具回「server 不可用」(已有 fail-soft 路径)。

use std::path::Path;

/// 一个语言的 LSP 接入描述 (静态表项)。
#[derive(Debug, Clone, Copy)]
pub struct LangServer {
    /// LSP `languageId` (didOpen 用): "rust" / "go" / "python" / "typescript" / "c" …
    pub language_id: &'static str,
    /// 服务器可执行名 (在 PATH 上找)。
    pub server_cmd: &'static str,
    /// 服务器命令行参数 (如 `--stdio`)。
    pub server_args: &'static [&'static str],
    /// 工程根标志文件 (向上找到第一个含此文件的目录 = **最近模块根**)。
    pub root_markers: &'static [&'static str],
    /// **workspace 聚合标志**: 若上行链中存在含此标志的目录, 取**最外层**那个当根 (多模块 monorepo);
    /// 否则回退「最近模块根」。`None` = 永远用最近根。Rust 特例: 聚合标志是 `Cargo.toml` 但需含 `[workspace]`。
    pub workspace_marker: Option<&'static str>,
}

const RUST: LangServer = LangServer {
    language_id: "rust",
    server_cmd: "rust-analyzer",
    server_args: &[],
    root_markers: &["Cargo.toml"],
    workspace_marker: Some("Cargo.toml"), // 仅含 [workspace] 的才算聚合
};
const GO: LangServer = LangServer {
    language_id: "go",
    server_cmd: "gopls",
    server_args: &[],
    root_markers: &["go.mod", "go.work"],
    workspace_marker: Some("go.work"), // 只有 go.work 提升到最外层; 否则用最近 go.mod (review fix #12)
};
const PYTHON: LangServer = LangServer {
    language_id: "python",
    server_cmd: "pyright-langserver",
    server_args: &["--stdio"],
    root_markers: &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", "Pipfile"],
    workspace_marker: None,
};
const TYPESCRIPT: LangServer = LangServer {
    language_id: "typescript",
    server_cmd: "typescript-language-server",
    server_args: &["--stdio"],
    root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    workspace_marker: None,
};
const JAVASCRIPT: LangServer = LangServer {
    language_id: "javascript",
    server_cmd: "typescript-language-server",
    server_args: &["--stdio"],
    root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    workspace_marker: None,
};
const C: LangServer = LangServer {
    language_id: "c",
    server_cmd: "clangd",
    server_args: &[],
    root_markers: &["compile_commands.json", ".clangd", "CMakeLists.txt", "Makefile"],
    workspace_marker: None,
};
const CPP: LangServer = LangServer {
    language_id: "cpp",
    server_cmd: "clangd",
    server_args: &[],
    root_markers: &["compile_commands.json", ".clangd", "CMakeLists.txt", "Makefile"],
    workspace_marker: None,
};

/// 按文件扩展名选语言服务器。未知扩展 → `None`。
pub fn lang_for_extension(ext: &str) -> Option<LangServer> {
    let e = ext.to_ascii_lowercase();
    Some(match e.as_str() {
        "rs" => RUST,
        "go" => GO,
        "py" | "pyi" => PYTHON,
        "ts" | "tsx" | "mts" | "cts" => TYPESCRIPT,
        "js" | "jsx" | "mjs" | "cjs" => JAVASCRIPT,
        "c" => C,
        // `.h` 走 C++ (clangd 对歧义头的默认即 C++, 也是混合/ C++ 代码库的安全默认; review fix #19)。
        "h" | "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" => CPP,
        _ => return None,
    })
}

/// 文件路径 → 语言服务器 (按扩展名)。
pub fn lang_for_path(path: &Path) -> Option<LangServer> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    lang_for_extension(ext)
}

/// 文件路径 → LSP `languageId` (didChange 推送等用; 未知则 `"plaintext"`)。
pub fn language_id_for_path(path: &Path) -> &'static str {
    lang_for_path(path).map(|l| l.language_id).unwrap_or("plaintext")
}

/// 找文件所属的工程根:
/// - **最近模块根** = 向上第一个含 `root_markers` 任一标志的目录;
/// - 若上行链存在 `workspace_marker` 聚合标志 (go.work / 含 `[workspace]` 的 Cargo.toml), 取**最外层**那个;
/// - 二者皆无则退回文件所在目录。
///
/// 关键 (review fix #12): 聚合**只由 workspace_marker 提升**, 普通模块标志 (go.mod / 嵌套 Cargo.toml)
/// **不**提升 —— 否则 monorepo 里编辑 /repo/svc/foo.go 会被外层 /repo/go.mod 抢成 /repo, gopls 根选错。
pub fn workspace_root_for(file: &Path, lang: &LangServer) -> std::path::PathBuf {
    let mut nearest: Option<std::path::PathBuf> = None;
    let mut aggregator: Option<std::path::PathBuf> = None;
    for dir in file.ancestors().skip(1) {
        if nearest.is_none() && lang.root_markers.iter().any(|m| dir.join(m).is_file()) {
            nearest = Some(dir.to_path_buf());
        }
        if let Some(agg) = lang.workspace_marker {
            let is_aggregator = if lang.language_id == "rust" && agg == "Cargo.toml" {
                // Rust: 同名文件, 仅含 [workspace] 的才算聚合根。
                std::fs::read_to_string(dir.join(agg))
                    .map(|s| s.contains("[workspace]"))
                    .unwrap_or(false)
            } else {
                dir.join(agg).is_file()
            };
            if is_aggregator {
                aggregator = Some(dir.to_path_buf()); // 继续向上 → 最外层聚合胜出
            }
        }
    }
    aggregator
        .or(nearest)
        .unwrap_or_else(|| file.parent().map(Path::to_path_buf).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn extension_maps_to_expected_server() {
        assert_eq!(lang_for_extension("rs").unwrap().server_cmd, "rust-analyzer");
        assert_eq!(lang_for_extension("go").unwrap().server_cmd, "gopls");
        assert_eq!(lang_for_extension("py").unwrap().language_id, "python");
        assert_eq!(lang_for_extension("ts").unwrap().server_cmd, "typescript-language-server");
        assert_eq!(lang_for_extension("tsx").unwrap().language_id, "typescript");
        assert_eq!(lang_for_extension("cpp").unwrap().language_id, "cpp");
        assert_eq!(lang_for_extension("c").unwrap().server_cmd, "clangd");
        assert!(lang_for_extension("xyzzy").is_none());
    }

    #[test]
    fn typescript_server_passes_stdio_arg() {
        assert_eq!(lang_for_extension("ts").unwrap().server_args, &["--stdio"]);
        assert_eq!(lang_for_extension("py").unwrap().server_args, &["--stdio"]);
        assert!(lang_for_extension("rs").unwrap().server_args.is_empty());
    }

    #[test]
    fn language_id_for_unknown_is_plaintext() {
        assert_eq!(language_id_for_path(Path::new("a.rs")), "rust");
        assert_eq!(language_id_for_path(Path::new("a.unknownext")), "plaintext");
    }

    #[test]
    fn dot_h_maps_to_cpp() {
        assert_eq!(lang_for_extension("h").unwrap().language_id, "cpp");
        assert_eq!(lang_for_extension("c").unwrap().language_id, "c");
    }

    #[test]
    fn go_nested_module_uses_nearest_unless_go_work() {
        let base = std::env::temp_dir().join("syncode_lang_go_nested_test");
        let outer = base.join("repo");
        let inner = outer.join("svc");
        let _ = std::fs::create_dir_all(&inner);
        let _ = std::fs::write(outer.join("go.mod"), "module repo\n");
        let _ = std::fs::write(inner.join("go.mod"), "module repo/svc\n");
        let go = lang_for_extension("go").unwrap();

        // 无 go.work → 编辑 svc/foo.go 取最近模块 svc (而非被外层 repo/go.mod 抢走)。
        assert_eq!(workspace_root_for(&inner.join("foo.go"), &go), inner);

        // 加 go.work 到 outer → 聚合提升到 outer。
        let _ = std::fs::write(outer.join("go.work"), "go 1.21\n");
        assert_eq!(workspace_root_for(&inner.join("foo.go"), &go), outer);

        let _ = std::fs::remove_dir_all(&base);
    }
}
