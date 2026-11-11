# 第三方软件与来源说明

本仓库包含两个上游项目的代码历史，以及新的 Rust 实现。仓库整体按 GPL-3.0 发布。

## 上游项目

| 项目 | 位置 | 许可证 |
|---|---|---|
| [keii-2596/ClaudeGuard](https://github.com/keii-2596/ClaudeGuard) | `legacy/claudeguard` | MIT，版权归 ivblz / keii-2596 及贡献者 |
| [gylive/ccodex-sleep-state](https://github.com/gylive/ccodex-sleep-state) | `legacy/ccodex-sleep-state` | GPL-3.0，版权归 gylive 及贡献者 |

ClaudeGuard 的 MIT 条款与 GPL-3.0 兼容。两个上游的 `LICENSE` 文件都保留在各自目录。ClaudeGuard 原项目基于 [ivblz/ClaudeGuard](https://github.com/ivblz/ClaudeGuard) 的 fork。

## Rust 依赖

直接依赖及版本记录在 `Cargo.toml`，完整解析结果在 `Cargo.lock`。主要包括：

- Tokio、Axum、Hyper、Reqwest：MIT / Apache-2.0。
- Serde、serde_json、toml、toml_edit：MIT / Apache-2.0。
- rustls、aws-lc-rs：ISC / Apache-2.0 / MIT 等，详见依赖包内许可证。
- tray-icon、winit、muda、winreg：MIT / Apache-2.0。
- sysinfo、fs2、ipnet、base64、sha2、zstd、flate2、chrono：MIT / Apache-2.0 / BSD 类许可证。

再分发源代码或二进制时，请保留 `LICENSE`、本说明和 `Cargo.lock`，并遵守各依赖自带的许可证。

## 参考实现

- [storytold/craft-anything](https://github.com/storytold/craft-anything)：净室重建方法参考。本项目没有嵌入它的代码。
- [openai/codex](https://github.com/openai/codex)：Codex 配置 schema 与 SSE 事件分类的公开参考。
- [DouDOU-start/codex-state-kit](https://github.com/DouDOU-start/codex-state-kit)：issue #12 提到的状态记录思路参考。本项目没有复制它的代码。

这些项目没有为本项目提供背书。

## 商标

ChatGPT、OpenAI、Codex 是 OpenAI 的商标。本项目与 OpenAI 没有隶属关系。名称只用于说明兼容对象。
