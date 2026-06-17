//! SynCode DeepSeek client.
//!
//! 自建薄 client: 我们自己拥有 wire 上的 message 类型, 对 `messages` 序列化、
//! `reasoning_content` 三态、以及 context 裁切有**字节级控制** —— 这是
//! 「完全掌控 context 裁切」产品路线的落点 (架构 §1, §8)。
//!
//! 权威行为源: 内部的 DeepSeek API 使用指南 (未公开)。本 crate 把该文档里散落的行为约束
//! (§7.4/§7.5 的 reasoning_content 回传规则、§12 前缀缓存、keep-alive、
//! finish_reason 怪癖、错误重试分类) 编码成类型与函数, 让规则由类型系统兜底。
//!
//! 当前为骨架: 类型与签名立起来, 网络/解析逻辑为 `todo!()`。
#![allow(dead_code, unused_variables)]

pub mod client;
pub mod context;
pub mod error;
pub mod stream;
pub mod wire;

pub use client::{DeepSeekClient, DeepSeekConfig};
pub use error::{Error, Result};
