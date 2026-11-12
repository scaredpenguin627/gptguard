# 0.4.0（未发布）

- 默认入口改为 Rust/Slint 原生桌面向导，不再打开浏览器面板。
- 同一个 `GPTGuard.exe` 提供窗口、托盘、后台任务和隐藏 CLI，不再发布独立 `gptguard-cli.exe`。
- Release 改为单文件 Windows x64 安装器，安装器内置并校验 Mihomo，不要求用户另行下载内核。
- 首次启动自动检测本机代理，支持粘贴订阅或选择 YAML、URI、Base64 节点文件。
- Mihomo 运行时按 SHA-256 释放到用户目录，继续使用回环监听、Job Object、就绪握手和失败关闭。
- 配置从 v1 自动迁移到 v2；旧 `gptguard` provider 优先按事务备份恢复，避免 OpenAI 聊天出现 provider mismatch。
- 接管 Codex 时保持原 provider 身份，只临时修改 `openai_base_url` 或原 provider 的 `base_url`。
- 普通界面不展示管理端口、管理 token、命令行或 Mihomo 路径。

# 0.3.0

- 新增 `proxy_only` 系统级阻断：受保护 exe 始终只能访问回环代理，外部直连被 Windows 防火墙阻断。
- 管理员环境实测：外部连接失败、回环服务成功、停守护后规则保留、关闭防护后恢复。
- WFP 过滤器原型没有发布。直接写 ALE_AUTH_CONNECT 过滤器时回环放行未通过验收，不能拿未验证内核策略替换可用方案。
- 文档补充系统级阻断的边界：按 exe 路径，不覆盖复制 exe 和子进程。

# 0.2.1

- 增加无控制台的 GUI/托盘入口 `gptguard-gui.exe`，并随仓库提供 `windows/GPTGuard.exe`。
- 增加一键启动、管理员启动、开机自启和打开面板脚本。
- `.gitattributes` 标记 `legacy/**` 为 vendored，修正 GitHub 语言统计。
- 在管理员权限下实际验证 Windows 更新锁的 hosts 写入与恢复。

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
