//! 工具契约 (借鉴 Claude Code 设计 IP, 架构 §10)。
//!
//! 核心理念: **error message 是写给模型读的** —— 措辞为引导模型下一步, 不是给人看。

use crate::background::BackgroundRegistry;
use crate::file_state::FileStateCache;
use crate::fs_scope::SharedFsScope;
use crate::permission::ActionRequest;
use async_trait::async_trait;
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use syncode_llm::wire::{FunctionDef, ToolDef};
use syncode_lsp::LspManager;
use thiserror::Error;

/// 派生子 agent 的请求 (§5 sub-agent 编排): 一句任务描述 + 给子 agent 的 prompt。
#[derive(Debug, Clone)]
pub struct SubAgentRequest {
    /// 短描述 (展示 / 日志用)。
    pub description: String,
    /// 给子 agent 的实际任务 prompt。
    pub prompt: String,
}

/// 子 agent 派生器 (由 [`AgentLoop`](crate::agent::AgentLoop) 注入): 跑一个**嵌套 agent loop** 到收尾,
/// 只把它的最终回答返回 (中间工具调用不进编排者 context —— 这正是 sub-agent 的价值)。`Err` = 子 agent 失败。
/// 子 agent 继承父的审批器与 cap-std 写收容 (权限**不放宽**, 只会更紧), 且**不能再派子 agent** (深度 1)。
pub type SubAgentSpawner = Arc<
    dyn Fn(SubAgentRequest) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// 工具执行错误。`Display` 文案直接回给模型, 故措辞要利于模型自纠偏。
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("execution failed: {0}")]
    Exec(String),
    #[error("permission denied: {0}")]
    Denied(String),
}

/// 一次文件改动的 unified diff (编辑类工具产出, 供 UI 渲染 diff 视图)。**不进**回给模型的 content。
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// 改动的文件路径 (展示用)。
    pub path: String,
    /// unified diff 文本 (`@@` hunk 头 + `+`/`-`/` ` 前缀行)。
    pub unified: String,
}

/// 工具一次调用的产出。`content` 是回给模型的字符串 (§11.1)。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    /// 是否为错误结果 (回给模型时可据此标注)。
    pub is_error: bool,
    /// 可选: 本次文件改动的 unified diff (编辑类工具产出)。仅供 UI 渲染, **不回给模型**。
    pub diff: Option<FileDiff>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, diff: None }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, diff: None }
    }
    /// 附带本次改动的 unified diff (供 UI; 不影响给模型的 content)。
    pub fn with_diff(mut self, diff: FileDiff) -> Self {
        self.diff = Some(diff);
        self
    }
}

/// 工具运行时上下文 (借鉴 CC `ToolUseContext`): 携带跨工具共享的状态。
#[derive(Clone)]
pub struct ToolCtx {
    /// 共享文件读状态缓存: Read 写、Edit/Write 读 —— 据此做「必先读」+ stale 检测 (§10)。
    pub files: Arc<FileStateCache>,
    /// 当前工作目录 (相对路径解析 / Grep 默认根)。
    pub cwd: PathBuf,
    /// 共享 LSP 客户端管理器: 跨工具调用复用常驻语言服务器 (§4 代码智能 / §6.2 机制③)。
    pub lsp: Arc<LspManager>,
    /// 文件写收容守卫 (P1c)。`None` = 不收容 (测试 / standalone)。写类工具经 [`ensure_writable`](Self::ensure_writable) 过它。
    pub fs: SharedFsScope,
    /// 后台任务注册表 (§5.5): Bash 的 run_in_background 在此登记, `BashOutput` 据此查/杀。跨工具调用共享。
    pub background: Arc<BackgroundRegistry>,
    /// 子 agent 派生器 (§5 编排)。`None` = 此上下文不允许派子 agent (子 agent 内部即如此 → 深度 1)。
    /// 由 `AgentLoop` 在启用 sub-agents 时注入。
    pub sub_agent: Option<SubAgentSpawner>,
}

impl ToolCtx {
    /// standalone / 测试用: 自带一个空的 LspManager, 不挂写收容。
    pub fn new(files: Arc<FileStateCache>, cwd: PathBuf) -> Self {
        Self::with_lsp(files, cwd, Arc::new(LspManager::new()))
    }

    /// agent loop 用: 注入**共享**的 LspManager, 全 turn 复用同一组常驻服务器 (持久活状态)。
    /// `fs` 默认 None、`background` 默认新建空的; agent loop 在 dispatch 里直接字段赋值换成**共享**实例
    /// (`ctx.fs = ...; ctx.background = ...`), 故同一 AgentLoop 内跨 dispatch 一致。
    pub fn with_lsp(files: Arc<FileStateCache>, cwd: PathBuf, lsp: Arc<LspManager>) -> Self {
        Self {
            files,
            cwd,
            lsp,
            fs: None,
            background: Arc::new(BackgroundRegistry::new()),
            sub_agent: None,
        }
    }

    /// 写类工具 (Write/Edit/AstEdit) 在落盘前调用: 挂了写收容则校验路径在授权根内, 否则放行。
    /// 越界返回 model-readable 的 `Denied` 错误 (写给模型读, 利于自纠偏)。
    pub fn ensure_writable(&self, path: &Path) -> Result<(), ToolError> {
        if let Some(scope) = &self.fs {
            scope.check_writable(path).map_err(|e| ToolError::Denied(e.to_string()))?;
        }
        Ok(())
    }

    /// 派生一个子 agent 跑完 `req` 并返回其最终回答。无派生器 (深度 1 边界 / 未启用) → model-readable 错误。
    pub async fn spawn_sub_agent(&self, req: SubAgentRequest) -> Result<String, ToolError> {
        match &self.sub_agent {
            Some(spawn) => spawn(req).await.map_err(ToolError::Exec),
            None => Err(ToolError::Exec(
                "sub-agent spawning is not available here (a sub-agent cannot spawn further sub-agents)"
                    .into(),
            )),
        }
    }
}

/// 工具契约。`Send + Sync` 以便放进 `Arc<dyn Tool>` 跨任务共享。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 稳定工具名 (进 `tool_calls.function.name`)。前缀稳定以吃缓存 (§12)。
    fn name(&self) -> &str;

    /// 写给模型读的描述。
    fn description(&self) -> &str;

    /// 参数 JSON Schema (§11)。建议满足 strict 三要素 (§11.4)。
    fn parameters(&self) -> Value;

    /// 这次调用要执行的「语义动作」, 供审批 (§7.5)。返回 `None` = 安全、无需过闸 (读类工具默认如此)。
    /// 危险工具据 `args` 把自己分类 (Bash 按命令、Write/Edit 按目标路径), 让审批按可逆性 / 影响面判,
    /// 而不是「是不是命令」一刀切。分类是 UX/策略层, 真边界靠沙箱 (见 [`crate::permission`] 顶注)。
    fn classify(&self, _args: &Value) -> Option<ActionRequest> {
        None
    }

    /// 执行。`args` 是已从 `function.arguments` (JSON 字符串) 解析出的对象;
    /// `ctx` 携带共享文件状态缓存等运行时上下文 (§10 文件工具联动)。
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;

    /// 生成发给模型的工具定义。
    fn to_def(&self) -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: FunctionDef {
                name: self.name().to_string(),
                description: Some(self.description().to_string()),
                parameters: self.parameters(),
                strict: false,
            },
        }
    }
}
