//! 审批 / 权限 (架构 §7.5, §10): 按「语义动作类别 + 影响面」授权, 而非逐条命令。
//!
//! **两层 (职责分离)**:
//!   1. **分类** —— 每个 [`Tool`](crate::tool::Tool) 最懂自己, 用 `classify(args)` 把一次调用映射成
//!      [`ActionRequest`] (按可逆性 / 影响面定级所需的语义类别 + 操作数)。
//!   2. **决策** —— [`Approver`] 把 `ActionRequest` 判成 [`Decision`] (Allow / Deny / Ask)。
//!
//! **诚实约束 (团队性格)**: 命令 / 路径分类是 **UX / 策略层启发式, 不是安全边界** —— shell 能
//! `;` / `$(...)` / 别名绕过任何前缀解析。真边界是**沙箱** (cap-std 文件能力面 / OS 镣铐, 路线图 P1/P2)。
//! 审批器只决定「要不要叫人」: 它拦的是**外发 / 不可逆**动作 (push / 装包 / 联网 / 删项目外 / 提权),
//! **不**容纳「项目内任意代码执行」—— 那归沙箱。未识别一律 fail-closed → `Ask` (当前无交互通道 = 拒),
//! 所以分类漏判只会偏**保守** (多问 / 多拒), 不会偏放行。

use std::path::PathBuf;
use thiserror::Error;

/// 语义动作类别 (借鉴 CC prompt-based 权限; 把原来粗粒度的 `ArbitraryExec` 按可逆性 / 影响面细分)。
/// 授权按类别给, 而非逐命令。**Allow 档** (可逆 / 项目内) 与 **Ask 档** (外发 / 不可逆 / 未识别) 见
/// [`PolicyApprover::decide`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionClass {
    /// 读 / 探查, 不改状态 (ls / cat / grep / git status …)。
    ReadFs,
    /// 写文件。具体路径在 [`ActionRequest::target`]: 写根内 → 放行, 根外 → Ask。
    WriteFs,
    /// 编译 / 构建 (cargo build, rustc, make, go build, tsc …) —— 可逆、项目内。
    Build,
    /// 跑测试 (cargo test, pytest, go test …) —— 可逆、项目内。
    RunTests,
    /// 版本控制本地操作 (git status / diff / add / commit / checkout …) —— 仓库内可逆。
    VcsLocal,
    /// 版本控制外发 (git push, git remote set-url, cargo/npm publish …) —— 外发、不可逆。
    VcsPublish,
    /// 装依赖 / 改环境 (apt / brew / npm i / pip install / cargo add …) —— 供应链面。
    InstallDeps,
    /// 联网 (curl / wget / ssh / scp / git clone …)。
    Network,
    /// 不可逆破坏 (rm -rf 出根 / 绝对 / `~` / `..`, 覆盖项目外 …)。
    Destructive,
    /// 提权 / 系统级 (sudo / su / runas, shutdown / kill …)。
    Privileged,
    /// 直接执行某个二进制 (`./x`, `.\x.exe`, 绝对路径)。写根内 → 放行 (跑自己刚构建的产物),
    /// 根外 → Ask。注意: 真正约束「项目内任意执行」靠沙箱, 此处仅按影响面叫不叫人。
    LocalExec,
    /// 未识别的命令 / 任意执行 —— fail-closed → Ask。
    ArbitraryExec,
    /// 其它 (带标签)。
    Other(String),
}

/// 一次工具调用要执行的「语义动作」, 供审批 (§7.5)。
#[derive(Debug, Clone)]
pub struct ActionRequest {
    /// 语义类别。
    pub class: ActionClass,
    /// 发起工具名 (写进给模型的拒绝信息, 利于自纠偏)。
    pub tool: String,
    /// 影响面操作数: 写的路径 / 执行的二进制 / (回退) 完整命令 —— 供审批做根内/根外判定与展示。
    /// 对 `WriteFs` / `LocalExec`: `Some(路径)` 才参与根判定; `None` 视作项目内 → 放行。
    pub target: Option<String>,
}

impl ActionRequest {
    pub fn new(class: ActionClass, tool: impl Into<String>) -> Self {
        Self { class, tool: tool.into(), target: None }
    }
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Error)]
#[error("action denied: {0:?}")]
pub struct Denied(pub ActionClass);

/// 审批器抽象。决策只依赖 [`ActionRequest`]; UI / 交互档后续接入 (本步先做策略档)。
pub trait Approver: Send + Sync {
    fn decide(&self, req: &ActionRequest) -> Decision;
}

/// 开发期 / 测试用: 全部放行。**生产入口必须换成 [`PolicyApprover`]** (否则 Bash 等危险工具裸奔)。
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl Approver for AllowAll {
    fn decide(&self, _req: &ActionRequest) -> Decision {
        Decision::Allow
    }
}

/// 全拒 (测试 / 锁死档)。
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn decide(&self, _req: &ActionRequest) -> Decision {
        Decision::Deny
    }
}

/// 策略档审批器 (**autonomy-first**): 可逆 / 项目内默认放行不打扰, 不可逆 / 出沙箱才 `Ask`。
///
/// 持有「写根」白名单: 默认 = 项目根 + 系统临时目录 (借鉴 CC `allowWrite = ['.', tmpdir]` —— 构建产物 /
/// scratch 常落临时目录)。`WriteFs` / `LocalExec` 的路径落在任一写根内 → 放行, 否则 `Ask`。
///
/// `Ask` 在交互档接好前 = fail-closed 拒 (见 [`crate::agent`] 闸门); 所以这张表把**安全多数**放进
/// Allow 档, 让全自动跑得动, 只把外发 / 不可逆挡在 Ask。
pub struct PolicyApprover {
    write_roots: Vec<PathBuf>,
}

impl PolicyApprover {
    /// `project_root` = 授权项目根 (通常 = 启动目录)。默认追加系统临时目录为写根。
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let mut write_roots = vec![crate::pathutil::normalize(&project_root.into())];
        write_roots.push(crate::pathutil::normalize(&std::env::temp_dir()));
        Self { write_roots }
    }

    /// 追加一个写根 (如显式授权的额外目录)。
    pub fn with_write_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.write_roots.push(crate::pathutil::normalize(&root.into()));
        self
    }

    /// 目标路径是否落在某个写根内。`None` (无具体路径 / 相对项目内) → 视为根内。
    /// 走共享的 [`pathutil::within_any`](crate::pathutil::within_any), 与 `FsScope` 用同一套
    /// Windows-aware (verbatim 前缀剥离 + 大小写不敏感) 判定, 两闸不分歧 (review fix)。
    fn within_write_roots(&self, target: Option<&str>) -> bool {
        match target {
            None => true,
            Some(t) => crate::pathutil::within_any(&PathBuf::from(t), &self.write_roots),
        }
    }
}

impl Approver for PolicyApprover {
    fn decide(&self, req: &ActionRequest) -> Decision {
        use ActionClass::*;
        match &req.class {
            // 读 / 可逆 / 项目内 → 放行不打扰。
            ReadFs | Build | RunTests | VcsLocal => Decision::Allow,
            // 写 / 执行: 按影响面 —— 写根内放行, 根外 Ask。
            WriteFs | LocalExec => {
                if self.within_write_roots(req.target.as_deref()) {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
            // 外发 / 不可逆 / 改环境 / 提权 / 未识别 → 停下叫人 (当前无交互通道 = fail-closed 拒)。
            VcsPublish | InstallDeps | Network | Destructive | Privileged | ArbitraryExec
            | Other(_) => Decision::Ask,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(class: ActionClass) -> ActionRequest {
        ActionRequest::new(class, "t")
    }
    fn req_t(class: ActionClass, target: &str) -> ActionRequest {
        ActionRequest::new(class, "t").with_target(target)
    }

    #[test]
    fn allow_all_allows_everything() {
        assert_eq!(AllowAll.decide(&req(ActionClass::Privileged)), Decision::Allow);
    }

    #[test]
    fn policy_allows_reversible_in_project_classes() {
        let a = PolicyApprover::new("/proj");
        for c in [ActionClass::ReadFs, ActionClass::Build, ActionClass::RunTests, ActionClass::VcsLocal] {
            assert_eq!(a.decide(&req(c.clone())), Decision::Allow, "{c:?} should be allowed");
        }
    }

    #[test]
    fn policy_asks_on_outward_and_irreversible_classes() {
        let a = PolicyApprover::new("/proj");
        for c in [
            ActionClass::VcsPublish,
            ActionClass::InstallDeps,
            ActionClass::Network,
            ActionClass::Destructive,
            ActionClass::Privileged,
            ActionClass::ArbitraryExec,
            ActionClass::Other("x".into()),
        ] {
            assert_eq!(a.decide(&req(c.clone())), Decision::Ask, "{c:?} should ask");
        }
    }

    #[test]
    fn policy_write_inside_root_allows_outside_asks() {
        let a = PolicyApprover::new("/proj");
        assert_eq!(a.decide(&req_t(ActionClass::WriteFs, "/proj/src/main.rs")), Decision::Allow);
        assert_eq!(a.decide(&req_t(ActionClass::WriteFs, "/etc/passwd")), Decision::Ask);
        // 路径穿越回根内 → 规范化后仍判根内。
        assert_eq!(a.decide(&req_t(ActionClass::WriteFs, "/proj/src/../a.rs")), Decision::Allow);
        // 穿越出根 → Ask。
        assert_eq!(a.decide(&req_t(ActionClass::WriteFs, "/proj/../etc/x")), Decision::Ask);
        // 无具体路径 → 视为项目内 → 放行。
        assert_eq!(a.decide(&req(ActionClass::WriteFs)), Decision::Allow);
    }

    #[test]
    fn policy_localexec_inside_root_allows() {
        let a = PolicyApprover::new("/proj");
        assert_eq!(a.decide(&req_t(ActionClass::LocalExec, "/proj/target/debug/app")), Decision::Allow);
        assert_eq!(a.decide(&req_t(ActionClass::LocalExec, "/usr/bin/curl")), Decision::Ask);
        assert_eq!(a.decide(&req(ActionClass::LocalExec)), Decision::Allow); // ./relative
    }

    #[test]
    fn policy_temp_dir_is_a_write_root() {
        let a = PolicyApprover::new("/proj");
        let tmp = std::env::temp_dir().join("scratch.rs");
        assert_eq!(
            a.decide(&req_t(ActionClass::WriteFs, &tmp.to_string_lossy())),
            Decision::Allow
        );
    }
}
