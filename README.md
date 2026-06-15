# SynCode — 参考文档库 / Reference Docs

> 自建 agent 编程工具 **SynCode** 的核心参考文档集合。

## 文档索引 / Index

| 文档 | 内容 |
|------|------|
| [架构与工具策略.md](架构与工具策略.md) | **共识固化活文档**。SynCode 的语言/框架选型、Rust 工具上限评级、"低于 shell"的系统级工具菜单（含激进/低回报项,全部存档标注）、**agent→系统交互的五类机制（shell / 进程内库 / 持久服务 RPC / 嵌入脚本 VM / 动态编译）+ code-as-action 三分流**、安全与权限架构（沙箱底座 + 特权 broker）、gpui 风险、从 Claude Code 借鉴的工具设计 IP、以及诚实的反方意见。含决策记录 / 开放问题 / 待办与关键 crate 速查。 |
| [DeepSeek-API-使用指南.md](DeepSeek-API-使用指南.md) | DeepSeek `deepseek-v4-pro` API 完整使用指南（中英对照）。含 context 工程要点：上下文硬盘缓存的整前缀单元命中规则（§12）、思考模式 `reasoning_content` 多轮回传规则（§7.4）、用空串 `""` 在保留在途轮的同时回收 CoT token 的**实测**裁切方法（§7.5）。 |

## 背景 / Context

SynCode 走「**自建 agent loop + 完全掌控 context 裁切**」路线——基于 raw Messages API（OpenAI 兼容口 / `https://api.deepseek.com`），而非 Agent SDK，以便对 `messages`、工具调用、CoT 做任意裁切（删整轮、置工具结果存根、置 `reasoning_content` 为 `""` 等）。各参考文档为该路线沉淀可直接落地的 API 行为与裁切规则。

## 许可证 / License

本项目以 **AGPL-3.0-only** 开源（强 copyleft + 网络条款：通过网络提供的修改版亦须开源）。完整条款见 [LICENSE](LICENSE)。
