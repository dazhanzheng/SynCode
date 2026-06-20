//! 路径「在根内」判定 (写收容 / 审批**共用**, review fix)。
//!
//! 统一两处对同一授权根的判定: `PolicyApprover` (词法策略) 与 `FsScope` (canonicalize 写收容)。
//! 若各算各的, Windows 上会分歧 —— 一个 Allow、一个 Ask, 或同一根被判内 / 外不一致。共用此处后两者一致:
//! - **Windows**: 剥离 `\\?\` verbatim 前缀 (canonicalize 产物带它, 词法路径不带 → 否则永不相等),
//!   大小写不敏感 (模型常发小写盘符 / 全小写路径), 统一反斜杠。
//! - **Unix**: 大小写敏感、原样比较。

use std::path::{Component, Path, PathBuf};

#[cfg(windows)]
const SEP: &str = "\\";
#[cfg(not(windows))]
const SEP: &str = "/";

/// 词法归一 (解析 `.`/`..`, **不碰文件系统**): 对不存在的新文件也稳, 不引入符号链接 TOCTOU。
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` 是否落在 `root` 之内 (含相等)。两侧都先词法归一再按平台规则比较。
pub fn within(path: &Path, root: &Path) -> bool {
    let p = comparable(&normalize(path));
    let r = comparable(&normalize(root));
    if r.is_empty() {
        return false;
    }
    let prefix = if r.ends_with(SEP) { r.clone() } else { r.clone() + SEP };
    p == r || p.starts_with(&prefix)
}

/// `path` 是否落在 `roots` 中任一根之内。
pub fn within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| within(path, r))
}

/// 归一成可比较字符串。Windows: 剥 verbatim 前缀 + 小写 + 统一反斜杠; Unix: 原样。
fn comparable(p: &Path) -> String {
    let s = p.to_string_lossy();
    #[cfg(windows)]
    {
        let stripped = match s.strip_prefix(r"\\?\") {
            Some(rest) => match rest.strip_prefix("UNC\\") {
                Some(unc) => format!(r"\\{unc}"),
                None => rest.to_string(),
            },
            None => s.to_string(),
        };
        stripped.to_ascii_lowercase().replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn within_basic() {
        assert!(within(Path::new("/proj/src/main.rs"), Path::new("/proj")));
        assert!(within(Path::new("/proj"), Path::new("/proj")));
        assert!(!within(Path::new("/projector/x"), Path::new("/proj"))); // 不是前缀目录
        assert!(!within(Path::new("/etc/passwd"), Path::new("/proj")));
        assert!(within(Path::new("/proj/a/../b"), Path::new("/proj"))); // 归一后仍在内
        assert!(!within(Path::new("/proj/../etc/x"), Path::new("/proj"))); // 穿越出根
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_insensitive_and_verbatim_agnostic() {
        // 小写盘符 / 全小写路径 (模型常发) 仍判内。
        assert!(within(
            Path::new(r"c:\users\dnf\proj\src\main.rs"),
            Path::new(r"C:\Users\dnf\Proj")
        ));
        // verbatim 前缀 (canonicalize 产物) vs 普通路径 → 相等。
        assert!(within(Path::new(r"\\?\C:\Proj\file.rs"), Path::new(r"C:\Proj")));
        // 正斜杠混用。
        assert!(within(Path::new("C:/Proj/sub/x"), Path::new(r"C:\Proj")));
    }
}
