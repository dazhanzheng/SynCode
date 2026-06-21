//! 文件系统**写**收容 (路线图 P1c / §7.3): 让进程内文件工具 (Write/Edit/AstEdit) 在写时**逃不出授权根**。
//!
//! 与审批器 (`PolicyApprover`) 的分工:审批器按**词法**判「写根内/外 → 放不放行」(策略层);
//! 本守卫做**构造级**收容——除词法归一外, 还对路径的**已存在祖先 canonicalize** 解析符号链接,
//! 挡住「根内有个符号链接指向根外, 写它就逃出去」这类**符号链接逃逸** (审批器只看词法、漏这一类)。
//!
//! **两层防御**:
//! 1. 词法/canonicalize **检查** ([`check_writable`](FsScope::check_writable)): 快速、给模型可读错误,
//!    且与 `PolicyApprover` 共用同一套根判定 (两闸不分歧)。
//! 2. cap-std **构造级收容** ([`write_atomic`](FsScope::write_atomic)): 实际写盘走授权根的 `Dir` 句柄,
//!    相对路径喂给它, 物理上 `..`/符号链接都**逃不出**该目录树 —— 关闭检查式守卫的 symlink TOCTOU 窗口
//!    (校验后、IO 前换链也没用: open 在内核里就被 cap-std 钉死在根内)。这是路线图 P1c 的端态。
//!
//! 读不收容 (与 CC 一致:deny-default 只管**写**, 读放开;真正的外泄面是网络, 由沙箱另行管控)。
//! 读收容 (一旦放开网络后才有意义) 作为后续项。

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("write to {path} is outside the authorized roots; refusing (path escapes the workspace sandbox)")]
pub struct FsDenied {
    pub path: String,
}

/// 一个授权写根: canonicalize 值 (检查用) + 词法值 (算相对路径用) + 可选 cap-std `Dir` 句柄 (构造级收容用)。
#[derive(Debug, Clone)]
struct Root {
    /// canonicalize 后的根 (检查 `within_any` 用, 与 `PolicyApprover` 同一套判定)。
    canon: PathBuf,
    /// 词法归一的原始根 (把绝对目标路径换算成根相对路径用 —— 不带 verbatim 前缀, 组件对齐)。
    lexical: PathBuf,
    /// 打在该根上的 cap-std `Dir` 句柄 (TOCTOU-proof 写)。根目录暂不存在等导致打开失败时为 `None`,
    /// 此时退回检查式守卫 + 裸原子写 (功能不降, 只是少了构造级收容那层)。
    dir: Option<Arc<Dir>>,
}

/// 文件写收容守卫。持有一组授权写根 (canon + cap-std `Dir` 句柄)。
#[derive(Debug, Clone)]
pub struct FsScope {
    roots: Vec<Root>,
}

impl FsScope {
    /// 以 `project_root` 为写根新建;并追加系统临时目录 (构建产物 / scratch 常落那, 借鉴 CC `allowWrite`)。
    /// 写根尽量 canonicalize (解析符号链接); 解析失败 (如目录暂不存在) 则退回词法归一值。
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let mut s = Self { roots: Vec::new() };
        s.add_root(project_root.into());
        s.add_root(std::env::temp_dir());
        s
    }

    /// 追加一个授权写根 (如显式放开的额外目录)。
    pub fn with_write_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.add_root(root.into());
        self
    }

    fn add_root(&mut self, root: PathBuf) {
        let canon = root.canonicalize().unwrap_or_else(|_| crate::pathutil::normalize(&root));
        if self.roots.iter().any(|r| r.canon == canon) {
            return;
        }
        // 在 canon 根上开 cap-std Dir (ambient authority: 我们信任这个根)。根不存在则 None。
        let dir = Dir::open_ambient_dir(&canon, ambient_authority()).ok().map(Arc::new);
        self.roots.push(Root { canon, lexical: crate::pathutil::normalize(&root), dir });
    }

    /// 目标路径的**真实位置**是否落在某写根内。解析符号链接 (canonicalize 已存在的祖先) 后判定。
    /// 走共享的 [`pathutil::within`](crate::pathutil::within), 与 `PolicyApprover` 同一套 Windows-aware
    /// 判定 (verbatim 前缀剥离 + 大小写不敏感), 故两闸对同一根不分歧 (review fix)。
    pub fn check_writable(&self, path: &Path) -> Result<(), FsDenied> {
        let real = resolve_real(&crate::pathutil::normalize(path));
        if self.roots.iter().any(|r| crate::pathutil::within(&real, &r.canon)) {
            Ok(())
        } else {
            Err(FsDenied { path: path.display().to_string() })
        }
    }

    /// **构造级收容**的原子写 (TOCTOU-proof): 经词法检查后, 把绝对目标路径换算成某授权根的相对路径,
    /// 走该根的 cap-std `Dir` 句柄写盘 (temp-in-dir + fsync + rename), 物理上逃不出根树。
    /// 该根没有 `Dir` 句柄 (打开失败) 时退回裸原子写 (仍经词法检查兜底)。越界返回 [`FsDenied`]。
    pub fn write_atomic(&self, path: &Path, content: &[u8]) -> std::io::Result<()> {
        // 第一层: 词法/canonicalize 检查 (快、给模型可读错误)。
        self.check_writable(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e.to_string()))?;

        // 找到包含该路径的根 + 根相对路径。
        for r in &self.roots {
            let Some(rel) = crate::pathutil::strip_within(path, &r.lexical) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "refusing to write to the root directory itself",
                ));
            }
            return match &r.dir {
                Some(dir) => write_in_dir(dir, &rel, content), // 第二层: 构造级收容
                None => write_atomic_raw(path, content),        // 无 Dir 句柄 → 裸原子写 (已过词法检查)
            };
        }
        // check_writable 过了却没匹配到根 (canon vs lexical 边界) → 兜底裸原子写。
        write_atomic_raw(path, content)
    }
}

/// 经 cap-std `Dir` 句柄做 temp-in-dir + fsync + rename 的原子写。`rel` 是根相对路径;
/// cap-std 保证 `..`/符号链接都出不去该 `Dir` 树。
fn write_in_dir(dir: &Dir, rel: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = rel.parent() {
        if !parent.as_os_str().is_empty() {
            dir.create_dir_all(parent)?;
        }
    }
    let file_name = rel
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp_name = format!(".{}.syncode-tmp", file_name.to_string_lossy());
    let tmp_rel = match rel.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(&tmp_name),
        _ => PathBuf::from(&tmp_name),
    };
    {
        let mut f = dir.create(&tmp_rel)?;
        f.write_all(content)?;
        f.flush()?;
        f.sync_all()?;
    }
    // rename 覆盖既有目标 (std/cap-std 在两平台都替换)。
    dir.rename(&tmp_rel, dir, rel)?;
    Ok(())
}

/// 裸原子写 (无 cap-std Dir 句柄时的退回): 同目录 temp + fsync + rename。仅在词法检查已通过后调用。
fn write_atomic_raw(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::fs;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    let tmp = dir.join(format!(
        ".{}.syncode-tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 解析路径的真实物理位置:对**最长的已存在祖先** canonicalize (解析符号链接), 再接回不存在的尾段。
/// 这样既能处理「写一个尚不存在的新文件」(canonicalize 整条会失败), 又能挡住祖先里的符号链接逃逸。
fn resolve_real(norm: &Path) -> PathBuf {
    let mut existing = norm.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(n) => {
                tail.push(n.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            None => break,
        }
    }
    let mut real = existing.canonicalize().unwrap_or(existing);
    for seg in tail.iter().rev() {
        real.push(seg);
    }
    real
}

/// 可选地挂在 [`ToolCtx`](crate::tool::ToolCtx) 上。`None` = 不收容 (测试 / standalone, 退回裸 `std::fs`)。
pub type SharedFsScope = Option<Arc<FsScope>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_inside_root_ok_outside_denied() {
        // 用真实临时目录做根 (canonicalize 需要真路径)。
        let root = std::env::temp_dir().join("syncode_fsscope_test_root");
        let _ = std::fs::create_dir_all(&root);
        let scope = FsScope::new(&root);

        assert!(scope.check_writable(&root.join("a.txt")).is_ok());
        assert!(scope.check_writable(&root.join("sub/b.txt")).is_ok());
        // 词法穿越回根内 → 允许。
        assert!(scope.check_writable(&root.join("sub/../c.txt")).is_ok());
        // 穿越出根**且出 temp** → 拒 (temp 本身是写根, 故取 temp 的父目录之外)。
        if let Some(above_temp) = std::env::temp_dir().parent() {
            let escaped = above_temp.join("syncode_fsscope_OUTSIDE.txt");
            assert!(scope.check_writable(&escaped).is_err(), "above-temp should be denied");
        }
    }

    #[test]
    fn temp_dir_is_a_write_root() {
        let root = std::env::temp_dir().join("syncode_fsscope_proj");
        let _ = std::fs::create_dir_all(&root);
        let scope = FsScope::new(&root);
        // 临时目录本身是写根 → 允许 (demo 把产物写这)。
        assert!(scope.check_writable(&std::env::temp_dir().join("scratch.rs")).is_ok());
    }

    #[test]
    fn write_atomic_lands_inside_root_via_capstd() {
        let root = std::env::temp_dir().join("syncode_capstd_write_root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let scope = FsScope::new(&root);
        // 写一个嵌套新文件: cap-std Dir 句柄应自动建中间目录并原子落盘。
        let target = root.join("a/b/c.txt");
        scope.write_atomic(&target, b"hello capstd").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello capstd");
        // 覆盖既有文件 (rename 替换)。
        scope.write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_atomic_outside_root_is_denied() {
        let root = std::env::temp_dir().join("syncode_capstd_deny_root");
        std::fs::create_dir_all(&root).unwrap();
        let scope = FsScope::new(&root);
        // 词法穿越出根且出 temp → 拒 (PermissionDenied), 绝不落盘。
        if let Some(above_temp) = std::env::temp_dir().parent() {
            let escaped = above_temp.join("syncode_capstd_OUTSIDE.txt");
            let err = scope.write_atomic(&escaped, b"evil").unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "got: {err}");
            assert!(!escaped.exists(), "denied write must not touch disk");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_denied() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join("syncode_fsscope_symlink_test");
        let root = base.join("root");
        let outside = base.join("outside");
        let _ = std::fs::create_dir_all(&root);
        let _ = std::fs::create_dir_all(&outside);
        let link = root.join("escape");
        let _ = std::fs::remove_file(&link);
        // root/escape -> outside (根内的符号链接指向根外)。
        if symlink(&outside, &link).is_ok() {
            let scope = FsScope::new(&root);
            // 词法上 root/escape/evil.txt 在根内, 但 canonicalize 后落到 outside → 必须被拒。
            assert!(scope.check_writable(&link.join("evil.txt")).is_err());
        }
    }
}
