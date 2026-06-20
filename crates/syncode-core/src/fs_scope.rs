//! 文件系统**写**收容 (路线图 P1c / §7.3): 让进程内文件工具 (Write/Edit/AstEdit) 在写时**逃不出授权根**。
//!
//! 与审批器 (`PolicyApprover`) 的分工:审批器按**词法**判「写根内/外 → 放不放行」(策略层);
//! 本守卫做**构造级**收容——除词法归一外, 还对路径的**已存在祖先 canonicalize** 解析符号链接,
//! 挡住「根内有个符号链接指向根外, 写它就逃出去」这类**符号链接逃逸** (审批器只看词法、漏这一类)。
//!
//! **诚实约束 / 端态**:这是「检查式」守卫 (resolve → 校验 → IO), 理论上有极小的 symlink TOCTOU 窗口
//! (校验后、IO 前被换链)。**真正构造级、TOCTOU-proof 的端态是 cap-std 的 `Dir` 句柄式 FS**
//! (工具只持授权根的 `Dir`, 物理上 `..`/符号链接都出不去) —— 作为本守卫的精化项在路线图 P1c 追踪。
//! 读不收容 (与 CC 一致:deny-default 只管**写**, 读放开;真正的外泄面是网络, 另行管控)。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("write to {path} is outside the authorized roots; refusing (path escapes the workspace sandbox)")]
pub struct FsDenied {
    pub path: String,
}

/// 文件写收容守卫。持有一组**已 canonicalize 的写根**;`check_writable` 解析目标路径的真实位置,
/// 不在任一写根内即拒。
#[derive(Debug, Clone)]
pub struct FsScope {
    write_roots: Vec<PathBuf>,
}

impl FsScope {
    /// 以 `project_root` 为写根新建;并追加系统临时目录 (构建产物 / scratch 常落那, 借鉴 CC `allowWrite`)。
    /// 写根尽量 canonicalize (解析符号链接); 解析失败 (如目录暂不存在) 则退回词法归一值。
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let mut s = Self { write_roots: Vec::new() };
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
        if !self.write_roots.contains(&canon) {
            self.write_roots.push(canon);
        }
    }

    /// 目标路径的**真实位置**是否落在某写根内。解析符号链接 (canonicalize 已存在的祖先) 后判定。
    /// 走共享的 [`pathutil::within_any`](crate::pathutil::within_any), 与 `PolicyApprover` 同一套
    /// Windows-aware 判定 (verbatim 前缀剥离 + 大小写不敏感), 故两闸对同一根不分歧 (review fix)。
    pub fn check_writable(&self, path: &Path) -> Result<(), FsDenied> {
        let real = resolve_real(&crate::pathutil::normalize(path));
        if crate::pathutil::within_any(&real, &self.write_roots) {
            Ok(())
        } else {
            Err(FsDenied { path: path.display().to_string() })
        }
    }
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
