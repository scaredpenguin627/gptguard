# 架构说明

## 进程与端口

```text
Codex CLI
   │  HTTP/SSE (localhost)
   ▼
gptguard run ── 127.0.0.1:17841 ── Codex 网关
   │
   ├── 出口检测循环（HTTP + STUN）
   ├── Windows 执行层（hosts / 防火墙 / 进程）
   ├── 127.0.0.1:17842 ── 管理面板
   └── state.json / logs / backups

gptguard tray ── 读 state.json，调用 17842 API
```

一个守护进程，一个托盘客户端。托盘不持有执行层，关闭托盘不影响防护。

## 代码分层

| 模块 | 职责 |
|---|---|
| `src/config.rs` | 配置结构、路径、校验、CIDR、原子写 |
| `src/ipcheck.rs` | HTTP 与 STUN 出口检测 |
| `src/guard.rs` | hosts、防火墙、进程终止、更新锁、自启动 |
| `src/codex/turnstate.rs` | state 解析、策略、active/ready 状态机 |
| `src/codex/provider.rs` | Codex provider 解析、认证分类、33 块规则、JWT 提示 |
| `src/codex/config_tx.rs` | `config.toml` 接管、备份、恢复、模型写入 |
| `src/codex/routes.rs` | 出口客户端、订阅导入 |
| `src/codex/proxy.rs` | Responses/SSE 网关、探测、注入、限流暂停 |
| `src/service.rs` | 守护生命周期、检测循环、共享状态 |
| `src/web.rs`、`web/index.html` | 本机面板与 API |
| `src/tray.rs` | Windows 通知区 |
| `src/cli.rs` | 命令行入口 |

## 一次请求

```text
Codex POST /backend-api/codex/responses
  → Host / Origin 校验
  → 解压 gzip/zstd，限制 64 MiB
  → 读取 model；校准列表之外直接转发
  → 按 Authorization + Account-Id + model 找会话
  → 检查 401/403/429 暂停
  → 取 state 快照；没有就探测
  → 选择出口，注入 X-Codex-Turn-State
  → 转发上游，流式回传
  → 记录响应头 state 的观测结果
```

生成请求不会自动重放。响应 state 只影响后续请求的 active/ready 选择，不会直接覆盖当前 active。

## state 状态机

- `active`：当前使用的 state。
- `ready`：备选，等 active 失效、接近续补窗口或连续两次观测异常后晋升。
- `strikes`：同一 active 版本连续收到不符合规则的响应时累加。
- `version`：每次发布递增，避免晚到的响应影响新 active。

解析只检查：

- Base64URL 解码成功。
- 首字节为 `0x80`。
- 时间戳在 2020–2100 之间。
- `(len - 57) % 16 == 0`。
- 块数在策略允许集合内。

不验证密文，也不生成密文。

## 配置事务

接管顺序：

1. 读取并解析 `config.toml`。
2. 解析 provider 和认证方式。
3. 备份原文件到 `%LOCALAPPDATA%\GPTGuard\backups\`。
4. 用 `toml_edit` 写入 `model_provider = "gptguard"` 和 `[model_providers.gptguard]`。
5. 记录原文件哈希、安装后哈希、上游、认证类型、profile。
6. 原子替换。

恢复时比较当前哈希与安装后哈希。不一致就拒绝，加 `--force` 时先把当前文件另存为冲突备份。网关每个请求检查事务与文件，发现不一致返回 409。

## 安全边界

- 只监听回环地址。
- 网关拒绝带 `Origin` 或 `Sec-Fetch-Site` 的请求，避免浏览器页面直接调用。
- 面板要求 `x-gptguard-token`，并校验 Host 与 Origin。
- 不读取 `auth.json`，不刷新 OAuth，不把凭据写到磁盘。
- 日志不记录 Authorization、完整 state、订阅 URL 或代理凭据。
- 中转 provider 清除 ChatGPT 专用头；官方 OAuth 凭据误发到中转时本地拒绝。
- 401/403/429 暂停按凭据共享，切换模型不会重置。
- 采集会消耗额度。没有可用 state 时，严格模式直接失败；兜底模式只转发一次。

## 测试策略

单元测试覆盖纯逻辑：CIDR、hosts 段、state、provider、订阅、配置事务。

集成测试在 `tests/proxy.rs` 启动本地 mock 上游，验证：

- 辅助模型不被本地拒绝，请求体原样到达上游。
- 33 块 state 能被采集并注入下一次生成。
- SSE 响应能流式回传。

没有真实账号的自动化测试。真实链路的验收要由使用者完成。
