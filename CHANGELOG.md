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
