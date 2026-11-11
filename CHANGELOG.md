# 0.2.0

- 项目、二进制、配置路径、Windows 标记和 GitHub 仓库更名为 GPTGuard。
- 移除模型白名单硬编码。未知模型只检查语法，默认校准 `gpt-*` 和 `o*`。
- 预置 `gpt-6.1-sol`、`gpt-6.1-terra`、`gpt-6.2-astra`、`gpt-6.2-sol`。
- Codex 请求 `/models` 时自动学习新模型 id，写入 `codex.models`。
- 新增 `gptguard models list/add/remove`、`gptguard state list/add-block/remove-block`。
- 面板增加固定模型和额外 state 块数入口。
- 配置文件迁移到 `%APPDATA%\GPTGuard`，数据迁移到 `%LOCALAPPDATA%\GPTGuard`。

# 0.1.1

- 修复守护被强制结束后，以“防护关闭”状态重启时不会清理残留 hosts/防火墙阻断的问题。
- 修正合并说明中关于 PR 代码合入方式的表述。

# 0.1.0

首个合并版本。

- 从 ClaudeGuard 移植出口 IP 白名单、HTTP/STUN 检测、失败关闭、hosts/防火墙阻断、进程终止、登录自启动和托盘。
- 从 ccodex-sleep-state 移植 Codex 配置接管、turn-state 解析与注入、代理池、SSE 网关、401/403/429 暂停。
- 合入 33 块（780 字符）state 支持，个人规则接受 `{10,33}`，Team 规则接受 `{12,33}`。
- 合入辅助模型直通，修复 `codex-auto-review` 等请求被本地拒绝的问题。
- 修复 `requires_openai_auth = true` 且没有 `base_url` 的自定义 provider 被误判为中转的问题。
- 增加 Windows 自动更新锁定和固定模型。
- 增加本机 Web 面板、托盘菜单、状态文件与日志。
- 加入本地 mock 上游集成测试，覆盖辅助模型直通与 33 块 state 注入。
