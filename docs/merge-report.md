# 合并与上游问题清单

本文记录两个上游仓库的 PR、issue 和 fork 处理结果。状态分为：

- 已合入：行为已在新 Rust 代码中实现，并有测试或手工验收。
- 部分合入：核心行为已实现，界面或平台差异没有完全照搬。
- 未合入：明确不在本轮范围，原因写在备注里。
- 非代码：提问、求推荐或使用咨询。

## 基线

| 上游 | 基线提交 | 说明 |
|---|---|---|
| keii-2596/ClaudeGuard | `1e496c2` | macOS 菜单栏、Python 跨平台版、Windows WFP 助手 |
| gylive/ccodex-sleep-state | `dab37ce` | v0.4.0 之后的文档提交 |

两个仓库都以 subtree 形式导入 `legacy/`，保留提交历史与许可证。

## ClaudeGuard

| 项目 | 类型 | 内容 | 状态 | 落点 |
|---|---|---|---|---|
| PR #1 / issue #1，`5ee2eac` | 其他人的代码 | Windows 通知区托盘、中文菜单、登录启动、单实例 | 已合入 | `src/tray.rs`、`src/guard.rs` 的 `set_autostart`、`tray.lock` |
| 文档缺口 | 功能 | Windows 端自动更新锁定 | 已合入 | `src/guard.rs` 的 `update_lock` / `remove_update_lock` |
| 文档缺口 | 功能 | Windows 端固定模型 | 已合入 | `src/codex/config_tx.rs`、`gptguard model` |
| 上游 macOS 菜单栏 | 功能 | AppKit 菜单栏与 Swift 守护 | 未合入 | 新项目用跨平台托盘代替；macOS 托盘本轮不提供 |
| 上游 Windows WFP 助手 | 功能 | 按 exe 的 WFP 内核过滤 | 未合入 | 改为 `netsh advfirewall` 按 exe 规则；没有内核驱动 |
| 上游 Linux cgroup/nftables | 功能 | cgroup v2 + nftables | 未合入 | 本轮只做 Windows 执行层 |

ClaudeGuard 的出口检测、失败关闭、IP 白名单、进程终止和登录自启动已移植。托盘菜单状态用文字显示，没有复制 macOS 的图标颜色资源。

## ccodex-sleep-state

| 项目 | 类型 | 内容 | 状态 | 落点 |
|---|---|---|---|---|
| PR #14，`7ddefb1` | 其他人的代码 | 个人/Team 接受 33 块（780 字符）state | 已合入 | `src/codex/turnstate.rs`、`src/codex/provider.rs` |
| PR #11，`1f3b302` | 其他人的代码 | 校准列表之外的辅助模型直通 | 已合入 | `src/codex/proxy.rs` 的 `valid_model_name` / `is_calibration_model` |
| issue #13 | bug | 780 字符 state 被策略拒绝 | 已合入 | 与 PR #14 同一逻辑；单元测试覆盖 33 块 |
| issue #10 | bug | 自定义 `requires_openai_auth` provider 缺 `base_url` 被误判为中转 | 已合入 | `src/codex/provider.rs` 的 `resolve`；单元测试 `issue_10_...` |
| issue #12 | 功能 | 增加模型状态与请求日志 | 部分合入 | 面板显示出口、代理、state 会话数、请求数；日志页读本机日志。没有照搬 codex-state-kit 的图表 |
| issue #7 | bug | `codex-auto-review` 等辅助请求被 `unsupported_model` 拒绝 | 已合入 | 与 PR #11 同一逻辑 |
| PR #6，`201fd32` | 其他人的代码 | 本地 Clash + 多个落地出口的两跳链式采集 | 部分合入 | 支持多个单跳出口、订阅和固定出口；两跳链式 connector 未实现 |
| PR #2，`cc54b25` | 其他人的代码 | go-toml 数组表 panic | 已合入上游 | Rust 版不用 go-toml；接管测试覆盖 TOML 数组表和注释保留 |
| PR #3，`d7695cc` | 维护者代码 | WebUI 采集时间设置、固定出口修复 | 部分合入 | 配置项与接管逻辑已移植；采集时间没有全部开放到面板 |
| PR #4，`16afa62` | 维护者代码 | v0.4.0 代理池、固定 state、检测助手 | 部分合入 | 代理池、直连/固定路由、订阅、面板状态已实现；中转助手与远程压缩没有完整移植 |
| PR #1，`ba3a2cc` | 其他人的代码 | Linux 发布包 | 部分合入 | CI 会构建/测试 Linux；没有生成四平台发布包 |
| issue #8 | 非代码 | 能否做 CPA 插件版 | 未合入 | 本轮只做独立程序和 CLI |
| issue #5 | 非代码 | cockpit tools 能否使用 | 未合入 | 没有对应客户端集成 |
| issue #9 | 非代码 | 梯子推荐 | 未合入 | 与代码无关 |

## Fork 处理

ClaudeGuard 有 4 个 fork，ccodex-sleep-state 有 145 个 fork。没有逐个自动合并 fork。原因：

- fork 里常见的是改配置、换图标、私有节点和未整理的实验代码，直接合并会带入凭据或无关改动。
- 有明确说明、可审查的改动已经通过 PR 分支提交。PR #1、#11、#14 都是其他人的代码，已按 PR 分支审阅后移植到 Rust。
- 需要某个 fork 的特定修复时，请给出 fork URL 和提交号，我再按同样方式审查。

## 模型升级策略

旧版把 `gpt-6-astra`、`gpt-5.6-sol`、`gpt-5.6-terra` 写死在代码里，新增模型会被当成未知模型或直接拒绝。新版改成三层：

1. `codex.models` 是提示目录，不是访问白名单。未知 model id 只检查语法，照常转发。
2. 校准按前缀分类。默认 `gpt-*`、`o*` 进入 state 采集，`codex-*` 和 `auxiliary_models` 直通。
3. Codex 请求 `/models` 时，网关读取响应并自动把新 id 写入 `codex.models`。

已经预置 6.1 和 6.2 的名字：

```text
gpt-6.1-sol
gpt-6.1-terra
gpt-6.2-astra
gpt-6.2-sol
```

新模型改变 state 块数时，用 `gptguard state add-block <n>` 追加，不改代码。

## 本轮没有做的事

- 没有逆官方 Codex/ChatGPT 客户端。state 是服务端密文，逆客户端不能合成合法 state。
- 没有调用真实 OpenAI 账号做自动化验收。所有网关测试使用本地 mock 上游。
- 没有实现 WFP、nftables、macOS Network Extension。
- 没有实现任意协议转换、WebSocket 或 Mihomo 全协议栈。
- 没有承诺提升模型质量，也没有把 292/332/780 当成质量指标。
