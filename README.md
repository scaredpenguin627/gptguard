# ChatGPTGuard / AIGuard

面向 ChatGPT 和 Codex 的本地出口守卫与 turn-state 转发器。Windows 优先，Rust 实现。

这个仓库把两个停更项目合到一起：

- [keii-2596/ClaudeGuard](https://github.com/keii-2596/ClaudeGuard)：出口 IP 白名单、失败关闭、hosts 与防火墙阻断、托盘、登录自启动。
- [gylive/ccodex-sleep-state](https://github.com/gylive/ccodex-sleep-state)：Codex 配置接管、`X-Codex-Turn-State` 采集与注入、代理池、HTTP/SSE 网关。

ClaudeGuard 的目标从 Claude 改成 ChatGPT 和 Codex。两个项目的 Git 历史都保留在 `legacy/` 下。

> 这个项目与 OpenAI 没有隶属关系。它不保证提升回答质量，不保证取得合格 state，也不绕过账号权限、额度或上游限流。使用前请自行确认账号条款和当地法律。

## 解决什么问题

Codex 的请求会带一个 `X-Codex-Turn-State` 响应头。上游偶尔下发不同长度、不同有效期的密文。本项目做三件事：

1. 用短探测请求从候选出口采集 state，按账号和模型隔离保存。
2. 正式生成时注入可用的 state，让请求走对应出口。
3. 出口 IP 不在白名单时，阻断 ChatGPT/Codex 程序，避免账号从意外地区发出请求。

state 是服务端签发的不透明密文。本项目只解析长度、时间戳和有效期，不伪造，也不把长度当成质量指标。

## 功能

| 模块 | 能力 |
|---|---|
| 出口检测 | HTTP 与 STUN 双通道；支持 `both`、`http`、`stun`；IPv4/IPv6 CIDR 白名单；检测失败按失败处理 |
| Windows 执行层 | hosts 阻断、按 exe 的 Windows 防火墙出站规则、终止目标进程；需要管理员权限 |
| 失败关闭 | 白名单为空、检测超时、state 采集失败都不会当成“已放行” |
| Codex 接管 | 备份原 `config.toml`，写入本地 provider，退出时恢复；接管后文件被改动会停止转发并返回 409 |
| turn-state | 解析 10/12/33 块；个人规则接受 `{10,33}`，Team 接受 `{12,33}`；active/ready 双槽与有效期 |
| 网关 | Responses、compact、models、alpha/search；SSE 流式转发；gzip/zstd 请求体 |
| 辅助模型 | 校准列表之外的模型原样转发，不注入 state；修复自动审批等辅助请求被本地拒绝的问题 |
| 上游拒绝 | 401/403/429 按凭据共享暂停；不换出口重试，不自动重放可能已计费的生成 |
| 代理池 | 直连、HTTP、HTTPS、SOCKS5；URI 列表、Base64 订阅、Clash/Mihomo YAML 子集 |
| Windows 更新锁 | hosts 更新域名 + 更新目录拒绝写入 ACL；需要管理员权限，关闭时恢复 |
| 固定模型 | 写入 Codex `config.toml` 的 `model`，并同步已接管的配置 |
| 托盘与面板 | Windows 通知区菜单；本机 Web 面板；状态、白名单、接管、采集、日志、自启动 |

## 快速开始

### 1. 构建

已安装 Rust 和 MSVC 链接器时：

```powershell
cargo build --release
```

只装了 Rust、没有 Visual Studio Build Tools 时，用 GNU 工具链：

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup run stable-x86_64-pc-windows-gnu cargo build --release
```

生成 `target\release\aiguard.exe`。把所在目录加入 PATH，或直接用完整路径运行。

### 2. 初始化

```powershell
aiguard setup
```

命令会创建 `%APPDATA%\AIGuard\config.json`，并打印管理口令和面板地址。

### 3. 填写可信出口

```powershell
aiguard add-ip 203.0.113.10
aiguard add-ip 198.51.100.0/24
aiguard set ip-check-mode both
```

白名单留空时，守护按失败处理。

### 4. 接入 Codex

先正常登录 Codex，确认不经过本工具也能使用。然后：

```powershell
aiguard codex takeover --auth chatgpt
```

接管会备份原文件，把 `model_provider` 改成 `aiguard`，并写入：

```toml
[model_providers.aiguard]
name = "ChatGPTGuard"
base_url = "http://127.0.0.1:17841/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
```

重启 Codex，新建会话。旧会话不会热切换。

恢复原配置：

```powershell
aiguard codex restore
```

如果接管后手动改过 `config.toml`，恢复会被拒绝。确认要覆盖时加 `--force`，当前文件会另存备份。

### 5. 启动

```powershell
aiguard run
```

或启动托盘。托盘会确保守护在运行：

```powershell
aiguard tray
```

需要修改 hosts、防火墙或更新锁时，在管理员终端里启动守护。普通权限下仍可检测出口和终止进程，但状态会显示执行层错误。

### 6. 打开面板

```powershell
aiguard web
```

默认地址 `http://127.0.0.1:17842`。口令在 `config.json` 的 `ui.admin_token`，`setup` 也会打印。

## 常用命令

| 命令 | 作用 |
|---|---|
| `aiguard setup` | 创建配置，显示下一步 |
| `aiguard run` | 启动守护：出口检测、Codex 转发、管理面板 |
| `aiguard tray` | 启动 Windows 托盘，并确保守护在运行 |
| `aiguard status` | 读取守护状态 |
| `aiguard check` | 立即检测出口；未通过时退出码为 1 |
| `aiguard doctor` | 检查权限、hosts、目标程序、配置目录、白名单 |
| `aiguard add-ip <IP/CIDR>` | 加入白名单 |
| `aiguard remove-ip <IP/CIDR>` | 移出白名单 |
| `aiguard list-ips` | 列出白名单 |
| `aiguard set <key> <value>` | 修改配置，见下表 |
| `aiguard model <model>` | 固定 Codex 模型 |
| `aiguard codex takeover` | 接管 Codex 配置 |
| `aiguard codex restore` | 恢复原配置 |
| `aiguard codex status` | 查看接管事务 |
| `aiguard codex probe` | 单独执行一次 state 采集 |
| `aiguard update-lock on\|off` | 开启或关闭自动更新锁定 |
| `aiguard autostart on\|off` | 开启或关闭登录自启动 |
| `aiguard guard -- <命令>` | 先检测出口，再启动命令；运行中失配会终止子进程 |

`aiguard set` 支持的键：

| 键 | 值 |
|---|---|
| `ip-check-mode` | `both`、`http`、`stun` |
| `interval` | 检测间隔秒数，1–300 |
| `model` | `gpt-6-astra`、`gpt-5.6-sol`、`gpt-5.6-terra` |
| `fallback` | `strict`、`passthrough` |
| `account-mode` | `auto`、`personal`、`team` |
| `fixed-model` | `true`、`false` |
| `network-guard` | `true`、`false` |
| `kill-on-block` | `true`、`false` |
| `injection` | `true`、`false` |
| `http-proxy` | 出口检测使用的 HTTP 代理；空字符串表示不用 |
| `codex-upstream` | 手动指定上游；空字符串表示跟随接管事务 |

## 配置与数据

| 内容 | Windows |
|---|---|
| 配置 | `%APPDATA%\AIGuard\config.json` |
| 状态 | `%LOCALAPPDATA%\AIGuard\state.json` |
| 日志 | `%LOCALAPPDATA%\AIGuard\logs\` |
| Codex 备份 | `%LOCALAPPDATA%\AIGuard\backups\` |
| 锁文件 | `%LOCALAPPDATA%\AIGuard\aiguard.lock`、`tray.lock` |
| Codex 配置 | `%USERPROFILE%\.codex\config.toml`，或 `CODEX_HOME` |

配置里的代理 URL 可能包含账号密码。面板返回时会打码。日志不记录完整 state、Authorization、订阅 URL 或代理凭据。

## turn-state 规则

| 账号规则 | 接受块数 | 通常长度 |
|---|---|---|
| 个人 | 10 或 33 | 292 或 780 |
| Team/Business | 12 或 33 | 332 或 780 |
| 显式旧版自定义基线 | 只接受原值 | 按原值 |

估计 TTL 为 3600 秒。最后 30 秒不再接纳。没有可用 state 时：

- `strict`：返回 503 `state_unavailable`，不转发。
- `passthrough`：本次请求不注入 state，照常转发一次，并加 `X-AIGuard-Mode: fallback-passthrough`。

两种模式都不会在收到上游响应后重放请求。

## 当前边界

这些地方与上游项目不同，或者还没做完：

- Windows 网络层用 `netsh advfirewall` 按 exe 阻断，没有移植 ClaudeGuard 的 WFP 内核助手。规则按程序路径生效，升级换路径后需要重新确认。
- 没有内嵌 Mihomo。代理只支持 HTTP、HTTPS、SOCKS5。Shadowsocks、VMess、VLESS、Trojan、Hysteria 等节点不会导入。
- PR #6 的“本地 Clash + 多个落地出口”两跳链式采集没有完整移植。当前支持多个单跳出口和订阅，出口模式仍是单跳。
- macOS 和 Linux 可以跑出口检测、Codex 转发和面板；hosts、防火墙、更新锁和托盘没有对应实现。
- 自动更新锁定是尽力而为：只处理配置里的域名和已存在的目录。ChatGPT 桌面端升级路径变化后要更新 `update_lock.paths`。
- ChatGPT 桌面端没有可从外部固定模型的接口。固定模型只写 Codex CLI 的配置。
- 采集会发真实请求并消耗额度。CI 只用本地 mock 上游，不调用真实账号。
- 不支持 WebSocket。升级请求返回 426。
- 模型名和 state 长度来自上游代码与社区反馈，不是 OpenAI 公布的接口契约。

## 上游合并

两个项目的完整历史已导入：

```text
legacy/claudeguard
legacy/ccodex-sleep-state
```

已合入的 PR、issue 修复和未合入项见 [docs/merge-report.md](docs/merge-report.md)。

重点修改：

- ClaudeGuard PR #1：Windows 托盘、登录启动、单实例。
- ccodex PR #14 / issue #13：接受 33 块（780 字符）state。
- ccodex PR #11 / issue #7：辅助模型请求直通。
- ccodex issue #10：`requires_openai_auth = true` 且没有 `base_url` 的自定义 provider 不再被误判为中转。
- ccodex issue #12：状态页与日志。
- ClaudeGuard 文档中的“自动更新锁定、固定模型 Windows 端尚未移植”：本仓库已实现，边界见上。

## 开发与测试

```powershell
cargo test
cargo build --release
```

测试覆盖：

- CIDR 与失败关闭。
- HTTP/STUN 解析。
- 10/12/33 块 state 解析、过期、未来时间、active/ready 晋升。
- provider 认证分类与 issue #10 回归。
- Codex 配置接管、外部改动拒绝、恢复。
- 订阅解析。
- 本地 mock 上游下的辅助模型直通与 33 块 state 注入。

没有真实账号的自动化验收。涉及真实 Codex 的功能需要你自己登录后验证。

## 许可证

GPL-3.0。ClaudeGuard 原项目为 MIT，ccodex-sleep-state 原项目为 GPL-3.0。两项目历史与许可证都保留在 `legacy/` 下。

第三方依赖说明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 相关文档

- [合并与上游问题清单](docs/merge-report.md)
- [Windows 使用与执行层](docs/windows.md)
- [架构说明](docs/architecture.md)
