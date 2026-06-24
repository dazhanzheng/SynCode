//! 安全/沙箱底座 (架构 §7)。
//!
//! 「激进授权的前提是先有这套底座」。骨架: 定义权限面 (`Policy`) 与给子进程上
//! 内核镣铐的抽象 (`Sandbox`); 各平台后端 (landlock/seccomp、Job Object、
//! sandbox_init) 随后填。当前提供一个 `NoopSandbox` 占位 (开发期)。
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox backend unsupported on this platform")]
    Unsupported,
    #[error("policy violation: {0}")]
    Policy(String),
    #[error("failed to apply sandbox: {0}")]
    Apply(String),
}

/// 一次受控执行的权限面 (capability surface)。默认收紧、显式放开 (§7.1)。
/// 模型能调的能力清单 = 它的权限边界 (§7.7)。
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub allow_network: bool,
    pub allow_exec: bool,
    /// 资源硬限 (Job Objects / cgroups, §7.4)。
    pub max_memory_bytes: Option<u64>,
    pub max_processes: Option<u32>,
    pub max_wall: Option<Duration>,
}

impl Policy {
    /// 最小权限: 啥也不放开。
    pub fn locked_down() -> Self {
        Self::default()
    }
}

/// 给 fork→exec 之间 / 子进程上「内核镣铐」的抽象 (§7)。平台后端各异 (§5.1 可移植税)。
pub trait Sandbox: Send + Sync {
    fn name(&self) -> &str;
    /// 在当前/子进程上施加 policy。须在 exec 前调用。
    fn apply(&self, policy: &Policy) -> Result<(), SandboxError>;
}

/// 占位实现: 不做任何隔离。仅开发期默认, 生产前必须换平台后端。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn name(&self) -> &str {
        "noop"
    }
    fn apply(&self, policy: &Policy) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// 进程容器 (spawn-into-container 模型, 区别于 `Sandbox::apply` 的 exec-前-施加模型)。
///
/// - **Windows**: Job Object —— spawn 后把进程 assign 进 job, **关 job handle 即杀整树**
///   (`KILL_ON_JOB_CLOSE`) + 活进程数/内存硬限。
/// - **Unix**: 进程组 —— 调用方 spawn 前给命令设 `process_group(0)` 让子进程自成组长, 超时
///   [`kill`](Self::kill) 时 `killpg(pgid, SIGKILL)` 杀整组 (Job Object 在 POSIX 的对位)。
///   ⚠️ **仅编译期 (cross-target check) 验证, 运行时待真 Linux/macOS 验**。资源硬限 / 能力面隔离
///   (cgroups/landlock/seccomp) 留给深度沙箱阶段。
///
/// 这是「跑模型给的命令」必须的底座: `cargo test` 会 spawn `rustc`/test 子进程, 朴素 kill 会留孤儿;
/// 容器杀的是整树 (§7.4)。
///
/// 用法: [`new`](Self::new) → spawn 后 [`contain`](Self::contain) (Windows 传 handle, Unix 传 pid)
/// → 超时/取消时 [`kill`](Self::kill)。
pub struct ProcessContainer {
    #[cfg(windows)]
    job: windows_backend::JobObject,
    #[cfg(unix)]
    pgid: std::sync::atomic::AtomicI32,
}

impl ProcessContainer {
    /// 按 `policy` 的资源硬限建一个容器。
    pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
        #[cfg(windows)]
        {
            Ok(Self {
                job: windows_backend::JobObject::new(policy)?,
            })
        }
        #[cfg(unix)]
        {
            let _ = policy;
            Ok(Self {
                pgid: std::sync::atomic::AtomicI32::new(0),
            })
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = policy;
            Ok(Self {})
        }
    }

    /// 把一个已 spawn 的进程纳入容器。Windows: 进程原生 `HANDLE`; Unix: 进程 `pid` (= 其进程组 pgid)。
    /// `id` 以 `isize` 传递以保持跨平台签名一致。
    pub fn contain(&self, id: isize) -> Result<(), SandboxError> {
        #[cfg(windows)]
        {
            self.job.assign(id)
        }
        #[cfg(unix)]
        {
            self.pgid.store(id as i32, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = id;
            Ok(())
        }
    }

    /// 杀掉容器内整棵进程树。Windows: `TerminateJobObject`; Unix: `killpg(pgid, SIGKILL)`。
    pub fn kill(&self) {
        #[cfg(windows)]
        {
            self.job.kill();
        }
        #[cfg(unix)]
        {
            let pgid = self.pgid.load(std::sync::atomic::Ordering::SeqCst);
            if pgid > 0 {
                unix_backend::kill_group(pgid);
            }
        }
    }
}

#[cfg(unix)]
mod unix_backend {
    //! Unix 进程组杀树 (`killpg`)。⚠️ 仅编译期 cross-check 验证, 运行时待真机验。
    /// 给整个进程组发 `SIGKILL` (子进程都已 `setpgid` 到同组, 故是杀整树)。
    /// 进程已退则 `ESRCH`, 无害。
    pub fn kill_group(pgid: i32) {
        unsafe {
            libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// **Unix 资源硬限**: 在子进程 (post-fork, pre-exec) 里施加 `setrlimit` —— 给 [`std::os::unix::process::CommandExt::pre_exec`]
/// 的闭包调用。`setrlimit` 是 async-signal-safe, 故可在 pre_exec 钩子里安全调用。
///
/// 当前只设 `RLIMIT_AS` (虚拟地址空间上限, 约束「单进程内存吃爆」), 且**仅 Linux** 施加。
/// **进程数上限**故意不走 `RLIMIT_NPROC` —— 它是**按真实 uid** 全局计数的, 设低会误伤用户自己其它进程的
/// fork, 危险; per-job 的进程数硬限需 cgroups v2 (路线图 P2, 待 Linux 真机)。Windows 侧用 Job Object 的
/// `ActiveProcessLimit` (见 [`windows_backend`])。
///
/// ⚠️ macOS/Darwin: `setrlimit(RLIMIT_AS, 有限值)` 被 XNU 拒绝 (EINVAL), 且即便接受也因 dyld/libmalloc
/// 预留巨量虚拟地址而形同虚设。**关键**: 本函数在 `pre_exec` 钩子里调用, 返回 `Err` 会中止 exec ——
/// 故在非 Linux 上一律 **no-op**, 绝不把「内存上限未生效」升级成「命令根本起不来」。macOS 的真实内存
/// 约束需另走 Seatbelt (见 [`macos`] backend, TODO)。
#[cfg(unix)]
pub fn apply_rlimits(max_memory_bytes: Option<u64>) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(bytes) = max_memory_bytes {
        let lim = libc::rlimit {
            rlim_cur: bytes as libc::rlim_t,
            rlim_max: bytes as libc::rlim_t,
        };
        // SAFETY: setrlimit 是 async-signal-safe; 在 pre_exec (单线程子进程) 中调用安全。
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &lim) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // 非 Linux unix (macOS 等): max_memory_bytes 不被强制 (见上)。消费掉参数避免未用告警。
    #[cfg(not(target_os = "linux"))]
    let _ = max_memory_bytes;
    Ok(())
}

#[cfg(windows)]
pub mod windows_backend {
    //! Job Object 真实实现 (windows-sys FFI, §4/§7)。
    use super::{Policy, SandboxError};
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobObject {
        handle: *mut c_void,
    }
    // job handle 仅由本结构独占持有, 跨线程移动安全。
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(SandboxError::Apply("CreateJobObjectW failed".into()));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            // 关 job 即杀整树 —— 容器消亡时不留孤儿。
            let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(max) = policy.max_processes {
                info.BasicLimitInformation.ActiveProcessLimit = max;
                flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            }
            if let Some(mem) = policy.max_memory_bytes {
                info.JobMemoryLimit = mem as usize;
                flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            }
            info.BasicLimitInformation.LimitFlags = flags;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return Err(SandboxError::Apply("SetInformationJobObject failed".into()));
            }
            Ok(Self { handle })
        }

        pub fn assign(&self, process: isize) -> Result<(), SandboxError> {
            let ok = unsafe { AssignProcessToJobObject(self.handle, process as *mut c_void) };
            if ok == 0 {
                return Err(SandboxError::Apply("AssignProcessToJobObject failed".into()));
            }
            Ok(())
        }

        pub fn kill(&self) {
            unsafe { TerminateJobObject(self.handle, 1) };
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE: 关 handle 即杀整树, 兜底防孤儿。
            unsafe { CloseHandle(self.handle) };
        }
    }
}

// 平台后端: Linux landlock(FS) + seccomp(网络) 已实现 (见下); macOS 待 (见末尾)。
#[cfg(target_os = "linux")]
pub mod linux {
    //! Linux 深度沙箱后端 (§4/§7, 支柱 4 = 解锁支柱 2 的前提): landlock LSM 做**文件能力收容**
    //! (让 `Policy.read_roots/write_roots` 首次真正 load-bearing), seccomp-BPF 做**网络拒绝**
    //! (让 `allow_network=false` 在**内核层**生效, 而非靠 classifier)。内核 ≥5.13 (landlock) / ≥3.5
    //! (seccomp), 均**无需 root**。
    //!
    //! ⚠️ **诚实约束 / 验证状态 (2026-06)**: 本后端**仅 cross-target 编译验证** (Windows 上
    //! `cargo check --target x86_64-unknown-linux-gnu`), **从未在真 Linux 内核上跑过逃逸测试** ——
    //! 内核是否真的挡住「写授权根外」「连任意网络」**尚未实证**。落到真 Linux/CI 前**不得**当作可信
    //! 隔离, 故**不**默认接进 Bash 执行路径 (默认仍 [`NoopSandbox`]); 这里只把后端**写出来 + 编译住**,
    //! 待 Linux CI 跑 [`escape_tests`] 文档化的两个逃逸用例 (写出根被拒 / curl 被拒, 均须在内核层失败)。
    //!
    //! **集成注意 (async-signal-safety)**: `PathFd::new` 会开 fd (分配, 非 async-signal-safe), 故
    //! landlock 规则集应在 **fork 前**构建好; 真正的 `restrict_self` / `apply_filter` 在子进程
    //! (post-fork, pre-exec) 调。当前 [`LinuxSandbox::apply`] 把两步合在一起 (供「在子进程里整体施加」
    //! 的调用模型), 真机接入时需按此切分 —— 这正是必须在 Linux 上验证的点之一。

    use super::{Policy, Sandbox, SandboxError};

    /// landlock + seccomp 后端。`apply` 须在**目标进程自身** (理想是子进程 post-fork、pre-exec) 调用。
    #[derive(Debug, Default, Clone, Copy)]
    pub struct LinuxSandbox;

    impl Sandbox for LinuxSandbox {
        fn name(&self) -> &str {
            "linux-landlock-seccomp"
        }

        /// 在当前进程上施加 policy: 先 landlock 文件收容, 再 (若禁网) seccomp 网络拒绝。
        /// 顺序无强制依赖, 但 landlock 先行可在 seccomp 之前就锁死 FS。
        fn apply(&self, policy: &Policy) -> Result<(), SandboxError> {
            apply_landlock(policy)?;
            if !policy.allow_network {
                apply_seccomp_no_network()?;
            }
            Ok(())
        }
    }

    fn map_ll(e: impl std::fmt::Display) -> SandboxError {
        SandboxError::Apply(format!("landlock: {e}"))
    }
    fn map_sc(e: impl std::fmt::Display) -> SandboxError {
        SandboxError::Apply(format!("seccomp: {e}"))
    }

    /// landlock 文件能力收容: 默认**无 FS 访问**, 仅显式放开 `read_roots` (只读) 与 `write_roots`
    /// (读写)。`BestEffort` 兼容老内核 (不支持则尽力降级, 不硬失败)。`write_roots` 同时含读权 (写需读)。
    fn apply_landlock(policy: &Policy) -> Result<(), SandboxError> {
        use landlock::{
            Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, ABI,
        };

        // ABI::V2 = 内核 5.13 的文件访问位集 (read/write/execute/remove/make 等)。
        let abi = ABI::V2;
        let access_read = AccessFs::from_read(abi);
        let access_all = AccessFs::from_all(abi); // 读+写+创建+删除…

        // 处理「所有文件访问位」(handle = 我们要管控的访问面), 然后逐根加规则放开。
        let mut created = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(access_all)
            .map_err(map_ll)?
            .create()
            .map_err(map_ll)?;

        for root in &policy.read_roots {
            let fd = PathFd::new(root).map_err(map_ll)?;
            created = created.add_rule(PathBeneath::new(fd, access_read)).map_err(map_ll)?;
        }
        for root in &policy.write_roots {
            let fd = PathFd::new(root).map_err(map_ll)?;
            created = created.add_rule(PathBeneath::new(fd, access_all)).map_err(map_ll)?;
        }

        created.restrict_self().map_err(map_ll)?;
        Ok(())
    }

    /// seccomp-BPF 网络拒绝: 默认放行全部 syscall, 仅对 `socket(AF_INET|AF_INET6, …)` 返回 `EACCES`
    /// (放过 `AF_UNIX`/`AF_NETLINK` 等本地族 —— 否则会误伤进程间通信)。挡住 IP socket 的创建 = 进程
    /// 无法发起 TCP/UDP 网络连接。
    fn apply_seccomp_no_network() -> Result<(), SandboxError> {
        use seccompiler::{
            apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
            SeccompCondition, SeccompFilter, SeccompRule,
        };
        use std::collections::BTreeMap;

        // socket 的第 0 个参数 = domain (AF_INET=2, AF_INET6=10)。命中即拒。
        let cond_inet = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_INET as u64,
        )
        .map_err(map_sc)?;
        let cond_inet6 = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_INET6 as u64,
        )
        .map_err(map_sc)?;

        let rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::from([(
            libc::SYS_socket,
            vec![
                SeccompRule::new(vec![cond_inet]).map_err(map_sc)?,
                SeccompRule::new(vec![cond_inet6]).map_err(map_sc)?,
            ],
        )]);

        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,                       // 默认: 放行 (不在 rules 里的 syscall)
            SeccompAction::Errno(libc::EACCES as u32),  // 命中: socket(AF_INET*) → EACCES
            target_arch()?,
        )
        .map_err(map_sc)?;

        let prog: BpfProgram = filter.try_into().map_err(map_sc)?;
        apply_filter(&prog).map_err(map_sc)?;
        Ok(())
    }

    fn target_arch() -> Result<seccompiler::TargetArch, SandboxError> {
        #[cfg(target_arch = "x86_64")]
        {
            Ok(seccompiler::TargetArch::x86_64)
        }
        #[cfg(target_arch = "aarch64")]
        {
            Ok(seccompiler::TargetArch::aarch64)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Err(SandboxError::Apply("seccomp: unsupported target_arch".into()))
        }
    }

    /// 文档化的逃逸测试契约 (真 Linux/CI 跑): 落地前必须人工/CI 验证, 因为本后端从未在内核上运行过。
    ///
    /// 1. **写收容**: policy.write_roots = [tmp_proj]; 子进程 `apply` 后, 写 `tmp_proj/ok.txt` 成功,
    ///    写 `/tmp/escape.txt` (根外) 必须 `EACCES`/`EPERM` —— **内核层**失败, 非 classifier。
    /// 2. **网络拒绝**: policy.allow_network = false; 子进程 `apply` 后, `socket(AF_INET, …)` 必须
    ///    返回 `EACCES` (等价: `curl http://example.com` 失败), 而 `AF_UNIX` socket 仍可建。
    ///
    /// 这两个用例无法在非 Linux 跑, 也不应在单元测试里 `restrict_self` (会污染测试进程), 故仅文档化。
    pub mod escape_tests {}
}

// 平台后端: macOS Seatbelt (sandbox_init + SBPL)。Linux 对位 = landlock+seccomp (见上)。
#[cfg(target_os = "macos")]
pub mod macos {
    //! macOS 深度沙箱后端 (§4/§7, Linux landlock+seccomp 在 Darwin 的对位): Seatbelt `sandbox_init`
    //! 配 SBPL 配置, 做**写收容** (让 `Policy.write_roots` 真正 load-bearing) + **网络拒绝**
    //! (让 `allow_network=false` 在**内核层**生效, 而非靠 classifier)。无需 root。
    //!
    //! **设计 (与项目「读不收容、只管写+网络」一致)**: 配置从 `(allow default)` 起步再逐项收紧 ——
    //! 而非 deny-default。Darwin 上 deny-default 会连 dyld / 系统库 / `/dev` 的读取都挡掉, 任何程序都
    //! 起不来 (Seatbelt 与 landlock 不同, 读不放开就寸步难行)。故只 `(deny file-write*)` 后放开授权写根
    //! 与 `/dev` 设备, 并按需 `(deny network*)`。读放开 (项目威胁模型: 外泄面是网络, 由网络拒绝兜)。
    //!
    //! **集成注意 (async-signal-safety)**: `sandbox_init` 会 malloc / 编译 SBPL, **非 async-signal-safe**,
    //! 故**不**默认接进 Bash 的 `pre_exec` 路径 (与 Linux 后端同, 默认仍 [`NoopSandbox`])。要在
    //! post-fork/pre-exec 施加, 需走「fork 前编译好配置 → 子进程仅做必要系统调用」的切分, 或用
    //! `posix_spawn` + 包装二进制。当前把后端写出来 + **在本机用 `sandbox-exec` 实证**配置正确
    //! (见 [`tests`]), 留待接入。
    //!
    //! `sandbox_init` 自 10.x 起标注 deprecated 但**至今可用** (Chromium / Bazel / sandbox-exec 在用);
    //! libc 未导出, 故下方自声明 FFI。

    use super::{Policy, Sandbox, SandboxError};
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::path::Path;

    // 公开 Seatbelt API (libSystem 默认链接)。`sandbox_init(profile, flags=0, &mut errbuf)`:
    // 0=成功; -1=失败且 errbuf 被填, 须 `sandbox_free_error` 释放。flags=0 表示 profile 是 SBPL 串。
    unsafe extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }

    // 私有 libsandbox API (sandbox-exec / Chromium 等长期在用, 稳定 ~15 年但无公开头文件)。这些符号**不在**
    // libSystem 默认导出里, 须显式 `#[link]` 到 SDK 的 libsandbox.tbd 存根。用于把「编译 SBPL」与「装载
    // profile」拆开: `sandbox_compile_string` 在**父进程 (fork 前)** 把 SBPL 编成不透明 profile (会
    // malloc/编译, **非** async-signal-safe); `sandbox_apply` 在**子进程 (fork 后、exec 前)** 只做一次
    // 「装载已编译 profile」的系统调用 (best-effort async-signal-safe) —— 能安全接进 `pre_exec` 的切分。
    #[link(name = "sandbox")]
    unsafe extern "C" {
        fn sandbox_compile_string(data: *const c_char, params: *mut c_void, error: *mut *mut c_char) -> *mut c_void;
        fn sandbox_apply(profile: *mut c_void) -> i32;
        fn sandbox_free_profile(profile: *mut c_void);
    }

    /// Seatbelt 后端。`apply` 须在**目标进程自身**调用 (理想是子进程 post-fork、pre-exec);
    /// `sandbox_init` 施加后**不可撤销**, 影响整个进程。
    #[derive(Debug, Default, Clone, Copy)]
    pub struct MacosSandbox;

    impl Sandbox for MacosSandbox {
        fn name(&self) -> &str {
            "macos-seatbelt"
        }

        fn apply(&self, policy: &Policy) -> Result<(), SandboxError> {
            let profile = compile_profile(policy);
            let c = CString::new(profile)
                .map_err(|e| SandboxError::Apply(format!("seatbelt: profile has interior NUL: {e}")))?;
            let mut err: *mut c_char = std::ptr::null_mut();
            // SAFETY: `c` 在调用期间存活; 失败时 `err` 指向 C 分配的串, 由 sandbox_free_error 释放。
            let rc = unsafe { sandbox_init(c.as_ptr(), 0, &mut err) };
            if rc != 0 {
                let msg = if err.is_null() {
                    "unknown error".to_string()
                } else {
                    // SAFETY: sandbox_init 失败时 err 指向有效 C 串。
                    let s = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
                    unsafe { sandbox_free_error(err) };
                    s
                };
                return Err(SandboxError::Apply(format!("sandbox_init failed: {msg}")));
            }
            Ok(())
        }
    }

    impl MacosSandbox {
        /// **父进程 / fork 前**: 把 [`Policy`] 编成已编译的 [`CompiledProfile`]。这一步 malloc + 编译
        /// SBPL, **非** async-signal-safe, 故必须在 fork 前调用; 产物移动进子进程的 `pre_exec` 钩子里
        /// 用 [`CompiledProfile::apply_in_child`] 装载。这是把 Seatbelt 安全接进 spawn 路径的正道
        /// (对照 Chromium: compile-in-parent / apply-in-child)。
        pub fn compile(policy: &Policy) -> Result<CompiledProfile, SandboxError> {
            let sbpl = compile_profile(policy);
            let c = CString::new(sbpl)
                .map_err(|e| SandboxError::Apply(format!("seatbelt: profile has interior NUL: {e}")))?;
            let mut err: *mut c_char = std::ptr::null_mut();
            // SAFETY: `c` 在调用期间存活; params=NULL (本 profile 无 `(param ...)` 占位符)。
            let handle = unsafe { sandbox_compile_string(c.as_ptr(), std::ptr::null_mut(), &mut err) };
            if handle.is_null() {
                let msg = if err.is_null() {
                    "unknown error".to_string()
                } else {
                    // SAFETY: 失败时 err 指向有效 C 串。
                    let s = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
                    unsafe { sandbox_free_error(err) };
                    s
                };
                return Err(SandboxError::Apply(format!("sandbox_compile_string failed: {msg}")));
            }
            Ok(CompiledProfile { handle })
        }
    }

    /// 父进程预编译好的 Seatbelt profile, 供子进程 `pre_exec` 装载。`Send`/`Sync`: 句柄由本结构独占,
    /// fork 后子进程经 COW 读到同一份仍有效。
    pub struct CompiledProfile {
        handle: *mut c_void,
    }
    unsafe impl Send for CompiledProfile {}
    unsafe impl Sync for CompiledProfile {}

    impl CompiledProfile {
        /// **子进程 / fork 后、exec 前**: 装载已编译 profile。只做一次系统调用 (不编译、不分配),
        /// best-effort async-signal-safe —— 能安全放进 `pre_exec` 的那一步。失败返回 `Err`, 调用方
        /// 应据此**中止 spawn** (要的沙箱装不上 = fail closed, 绝不让命令裸跑)。
        pub fn apply_in_child(&self) -> std::io::Result<()> {
            // SAFETY: handle 是 compile() 产出的有效 profile; fork 后 COW 内存仍有效。
            let rc = unsafe { sandbox_apply(self.handle) };
            if rc != 0 {
                return Err(std::io::Error::other("sandbox_apply failed"));
            }
            Ok(())
        }
    }

    impl Drop for CompiledProfile {
        fn drop(&mut self) {
            // SAFETY: handle 由本结构独占; 仅在父进程 drop (子进程 exec 后映像被替换, Drop 不会跑)。
            unsafe { sandbox_free_profile(self.handle) };
        }
    }

    /// 由 [`Policy`] 生成 SBPL 配置串。`pub` 供测试 (用 `sandbox-exec -p <profile>` 实证 Seatbelt 真收容)。
    ///
    /// 规则按「最后命中者胜」语义排布: 先 `(allow default)`, 再 `(deny file-write*)` 收掉所有写,
    /// 再放开 `/dev` 设备 (null/tty/pty —— 否则 shell/编译器写不了 stdio) 与各授权写根 (canonicalize,
    /// 解析 `/var`→`/private/var` 等符号链接, 与内核施加时的路径解析对齐), 最后按需 `(deny network*)`。
    pub fn compile_profile(policy: &Policy) -> String {
        let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        p.push_str("(allow file-write*\n    (subpath \"/dev\")\n");
        for root in &policy.write_roots {
            let canon = root.canonicalize().unwrap_or_else(|_| root.clone());
            p.push_str("    (subpath ");
            p.push_str(&sbpl_string(&canon));
            p.push_str(")\n");
        }
        p.push_str(")\n");
        if !policy.allow_network {
            p.push_str("(deny network*)\n");
        }
        p
    }

    /// 把路径编成 SBPL 字符串字面量 (双引号包裹, 转义 `\` 与 `"`)。
    fn sbpl_string(path: &Path) -> String {
        let s = path.to_string_lossy();
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            if ch == '\\' || ch == '"' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Command;

        /// 用系统的 `sandbox-exec`(同一 Seatbelt 引擎) 跑 `sh -c <cmd>`, 实证我们生成的配置。
        fn run_sandboxed(profile: &str, sh_cmd: &str) -> std::process::Output {
            Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg(profile)
                .arg("/bin/sh")
                .arg("-c")
                .arg(sh_cmd)
                .output()
                .expect("spawn /usr/bin/sandbox-exec")
        }

        #[test]
        fn profile_confines_writes_to_roots() {
            let root = std::env::temp_dir().join("syncode_seatbelt_write_root");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let policy = Policy { write_roots: vec![root.clone()], ..Policy::default() };
            let profile = compile_profile(&policy);

            // 1. 写授权根内 → 成功 (Seatbelt 放行)。
            let inside = root.join("ok.txt");
            let out = run_sandboxed(&profile, &format!("echo hi > '{}'", inside.display()));
            assert!(
                out.status.success() && inside.exists(),
                "in-root write must succeed; stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );

            // 2. 写根外 (temp 根目录, 不在 write_roots) → 必须被**内核层**拒, 不落盘。
            let outside = std::env::temp_dir().join("syncode_seatbelt_ESCAPE.txt");
            let _ = std::fs::remove_file(&outside);
            let out = run_sandboxed(&profile, &format!("echo evil > '{}'", outside.display()));
            assert!(!out.status.success(), "out-of-root write must be denied by Seatbelt");
            assert!(!outside.exists(), "denied write must not touch disk");

            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn profile_denies_network_when_disallowed() {
            let policy = Policy { allow_network: false, ..Policy::default() };
            let profile = compile_profile(&policy);
            assert!(profile.contains("(deny network*)"), "profile must deny network");
            // 连一个字面 IP (跳过 DNS), 短超时。deny network* 下 socket/connect 在内核层失败 → curl 非 0。
            let out = run_sandboxed(&profile, "curl -sS --max-time 3 http://93.184.216.34/ >/dev/null");
            assert!(!out.status.success(), "network must be denied under (deny network*)");
        }

        #[test]
        fn profile_allows_network_when_permitted() {
            // allow_network=true → 配置里不应有网络拒绝 (放行决策在更上层)。
            let policy = Policy { allow_network: true, ..Policy::default() };
            assert!(!compile_profile(&policy).contains("deny network"));
        }

        #[test]
        fn compile_split_produces_a_profile() {
            // 验证私有 libsandbox API 能链接 + SBPL 编译成功 (apply 不可撤销, 不能在测试进程里跑,
            // 故由 syncode-tools 的 BashTool 端到端测真正装载效果)。
            let root = std::env::temp_dir().join("syncode_seatbelt_compile_root");
            let _ = std::fs::create_dir_all(&root);
            let policy = Policy { write_roots: vec![root], ..Policy::default() };
            let _profile = MacosSandbox::compile(&policy).expect("compile seatbelt profile");
        }
    }
}
