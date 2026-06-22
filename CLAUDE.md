# SynCode

> 给在本仓库工作的 Claude agent 的行动纲领（operating charter）：**这是什么、我们在赌什么、做决策时按什么对齐、文档放哪 / 细节去哪查**。本文件**只承载高层纲领**——细节一律指向 `private-docs/`，不在此复制（避免过时副本）。

## 这是什么

SynCode = **纯 Rust、自建**的 AI coding agent。核心押注：**DeepSeek v4（OpenAI 兼容口）+ 在 raw Messages API 上自建 agent loop（不用任何 Agent SDK）**，以换取对 **context 的完全掌控**。它刻意区别于 shell-out / TypeScript 系 agent——靠 Rust 的 **in-process** 能力做出差异化，而非靠速度：把工具从「对文件字节操作」抬到「对程序的活语义模型操作」。

## 四大能力支柱（决策时按此对齐）

1. **自主、结构化裁切 + 智能压缩 context** —— 完全掌控 `messages` / tool-results / CoT 的取舍（删整轮、工具结果置存根、`reasoning_content` 置空回收 CoT token）。这是 SynCode 的**核心差异化**与**真正的「上限杠杆」**：context 压缩质量比工具速度重要得多。涉及裁切 / loop 的决策优先服务「控制力」，不为省事牺牲掌控。
2. **高自治 / 可激进授权地执行 agent 与 subagent** —— 敢把大幅自治权交给模型；**前提是安全底座（支柱 4）先到位**，没有底座就不放权。
3. **in-process 语义操作 + code-as-action** —— 工具跑在 agent **自己的进程内**，直接操作程序的**活语义模型**（LSP / tree-sitter AST / file-watch），而非裸字节 / 文本。code-as-action 默认走嵌入式脚本 VM（rhai）；重批量可现场把 Rust 编为 native；不可信计算编成 WASM 跑在 wasmtime 上。在恰当处**跨过 shell 抽象边界**（below-shell），换取自上而下、充分的系统管控力，可实时生成 Rust 级底层与 system 级代码。
4. **更高级、可回溯的沙箱** —— cap-std capability-FS + landlock/seccomp（Linux）/ Job Objects（Windows）+ privileged broker + overlayfs COW 可丢弃实验。原则：**「能安全地收紧 = 才敢激进授权」**。

## 立场与诚实约束（保持团队性格）

- **激进优先**：高风险 / 低回报的方向**保留在册并打标**，不提前删除。
- **智识诚实，反方意见留档**：Rust **不**抬高**权限**天花板（那由 OS 决定）；真正的天花板是**模型 + 工具设计 + context 压缩质量**；per-call 性能基本无关紧要（被 LLM 延迟淹没）。选 Rust 是为了：context 完全掌控、in-process 语义红利、让激进授权变安全的安全底座、以及单语言 / 单二进制的长期一致性——**不是因为「Rust 快」**。
- **平台**：macOS / Windows / Linux 桌面；移动端仅作驱动远程 agent 的瘦客户端（将来）。
- **借鉴策略**：从 `claude-code-main/` 吸取工具**设计 IP**（契约、写给模型读的 error message），用 Rust 重写并升级——不沿用其语言 / 实现。
- **Bash 永远是万能逃生口**；below-shell 工具只在「跨边界真回本」处外科手术式加入。

## 文档放哪 / 细节去哪查（务必遵守）

- **`docs/`** = **公开**。面向开源用户与其他开发者的功能 / 参考文档，随仓库以 **AGPL-3.0-only** 一起开源。
- **`private-docs/`** = **本地私有、不发布**。纲领性文件、私有思路、决策底账 / living docs：
  - `private-docs/架构与工具策略.md` —— **架构与工具策略的权威源**（选型、上限评级、五类 agent→系统交互机制、安全模型、决策记录 / 开放问题 / 待办）。
  - `private-docs/DeepSeek-API-使用指南.md` —— **DeepSeek API 行为与 context 裁切的权威源**（`reasoning_content` 回传规则、CoT token 回收、prompt cache 前缀等）。
- **本 `CLAUDE.md`** = **只放高层纲领**。需要细节时**去 `private-docs/` 查权威源**，不要把细节抄进本文件（避免读到过时副本）。
- **当前进度 / 状态**（早期可编译骨架、实现体多为 `todo!()`）与 crate 工作区结构以根 `README.md` 为准——**指向它，别在此复述**。
