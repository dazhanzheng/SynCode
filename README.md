# SynCode

> 纯 Rust 的自建 agent 编程工具 / A self-built AI coding agent in pure Rust.
> DeepSeek v4 + 自建 agent loop + 完全掌控 context 裁切。

> ⚠️ **状态 / Status:** 早期脚手架。当前是一个**可编译的 workspace 骨架**——结构、类型、
> 工具契约都已立起来,实现体多为 `todo!()`,尚不可用于实际编码。
> Early scaffold: a compilable workspace skeleton; most implementations are still `todo!()`.

## 这是什么 / What

SynCode 走「**自建 agent loop + 完全掌控 context 裁切**」路线——基于 raw Messages API
(OpenAI 兼容口 / `https://api.deepseek.com`),而非 Agent SDK,以便对 `messages`、工具调用、
CoT 做任意裁切(删整轮、置工具结果存根、置 `reasoning_content` 为 `""` 等)。

## 工作区结构 / Workspace

纯 Rust Cargo workspace,按关注点拆分:

| crate | 职责 / Responsibility |
|---|---|
| `syncode-llm` | DeepSeek 类型化 client:自有 wire 类型 + context 裁切原语 |
| `syncode-sandbox` | 安全 / 沙箱底座 trait |
| `syncode-core` | 自建 agent loop + 会话 + context 管理 + 工具 registry / 审批 |
| `syncode-tools` | 内置工具 (Read / Edit / Grep / Bash) |
| `syncode-cli` | headless 装配入口 (二进制 `syncode`) |

## 构建 / Build

需要 Rust stable (见 `rust-toolchain.toml`):

```sh
cargo build
cargo run -p syncode-cli   # 运行 headless 骨架入口
```

## 文档 / Documentation

公开文档见 [`docs/`](docs/)。

## 许可证 / License

本项目以 **AGPL-3.0-only** 开源(强 copyleft + 网络条款:通过网络提供的修改版亦须开源)。
完整条款见 [LICENSE](LICENSE)。
