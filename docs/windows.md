# Windows 使用与执行层

## 环境

- Windows 10/11，x64。
- Rust 1.85 或更新版本。没有 MSVC 链接器时用 GNU 工具链，见 README。
- 修改 hosts、Windows 防火墙、更新目录 ACL 需要管理员权限。
- 使用 Codex 前先完成官方登录，确认不经过本工具也能正常请求。

## 启动顺序

```powershell
gptguard setup
gptguard add-ip <可信出口 IP 或 CIDR>
gptguard codex takeover --auth chatgpt
gptguard run
```

需要托盘时用 `gptguard tray`。托盘会先启动守护。登录自启动写入：

```text
HKCU\Software\Microsoft\Windows\CurrentVersion\Run
GPTGuard = "<exe>" tray --start-guard
```

## 执行层做了什么

守护每轮做一次出口检测，按结果执行：

| 结果 | 动作 |
|---|---|
| 白名单通过 | 删除 GPTGuard 的 hosts 段和防火墙规则 |
| 出口不在白名单 | 写入 hosts 段、添加按 exe 的出站阻断规则、终止配置里的目标进程 |
| 检测失败 | 与“不在白名单”相同，按失败处理 |
| 防护关闭 | 删除 GPTGuard 的 hosts 段和防火墙规则 |

hosts 段用固定标记包起来：

```text
# BEGIN GPTGuard egress block
0.0.0.0 chatgpt.com
::1 chatgpt.com
...
# END GPTGuard egress block
```

只改这一段，不动其他行。防火墙规则名是 `GPTGuard Block <目标名>`，按 `program=<exe 路径>` 阻断出站。规则不按域名解析，代理连接也会被覆盖。

普通权限下，hosts 和防火墙会失败，状态变成 `error`，详情里带原因。进程终止不需要管理员权限，但只能终止当前用户有权限终止的进程。

## 自动更新锁定

`gptguard update-lock on` 做两件事：

1. 在 hosts 写入 `# BEGIN GPTGuard update lock` 段，覆盖配置里的更新域名。
2. 对配置里已存在的更新目录执行 `icacls <目录> /deny "<用户>:(OI)(CI)(W,D)"`。

关闭时删除 hosts 段，并按 `update-lock.json` 记录逐个执行 `/remove:d`。

默认域名：

```text
persistent.oaistatic.com
cdn.oaistatic.com
updates.chatgpt.com
update.chatgpt.com
```

默认目录：

```text
%LOCALAPPDATA%\Programs\ChatGPT
%LOCALAPPDATA%\ChatGPT
%LOCALAPPDATA%\Programs\codex
```

目录不存在时跳过。升级路径变化后要改 `config.json` 的 `update_lock.paths`。这个功能没有禁用计划任务或服务，也不是不可绕过的更新隔离。

## 固定模型

```powershell
gptguard model gpt-5.6-sol
```

命令做三件事：

1. 把 `codex.fixed_model` 设为 `true`。
2. 把 `codex.model` 设为指定模型。
3. 如果 Codex 配置已接管，直接改已安装配置里的 `model`，并更新事务哈希。

没有接管事务时只写本工具配置，下次 `codex takeover` 时再写入。

ChatGPT 桌面端没有可从外部固定模型的接口，这个功能只对 Codex CLI 生效。

## 托盘

菜单项：

- 状态：守护判定、HTTP 出口、Codex 接管状态。
- 立即检测出口。
- 打开设置面板。
- 开关防护、更新锁、登录自启动。
- 退出托盘，守护继续运行。

双击托盘图标打开面板。退出托盘不会停止守护；停止守护用 `Ctrl+C` 结束 `gptguard run`，或在任务管理器结束对应进程。

## 面板

默认 `http://127.0.0.1:17842`。只监听回环地址，API 需要 `x-gptguard-token`。页面地址带 `#token=` 片段，JavaScript 读取后写入 `sessionStorage`，随后清掉地址栏片段。

面板可以查看状态、改白名单、开关防护、接管/恢复 Codex、导入订阅、测试采集、开关更新锁和自启动、读日志。代理 URL 和凭据返回时打码。

## 常见问题

### 状态显示“执行层异常”

看 `gptguard status` 或面板里的 `enforcement.errors`。常见原因：

- 终端不是管理员，hosts 或防火墙写入失败。
- 目标程序路径不存在，`netsh` 拒绝添加规则。
- 防火墙服务被禁用。

### Codex 返回 409 `codex_config_changed`

接管后 `config.toml` 被其他工具改过。先退出 Codex，运行：

```powershell
gptguard codex status
gptguard codex restore --force
gptguard codex takeover --auth chatgpt
```

`--force` 会先把当前文件另存到备份目录，再写回接管前的内容。

### Codex 返回 503 `state_unavailable`

严格模式下没有采到合格 state。处理顺序：

1. 面板里看采集诊断和冷却时间。
2. 确认账号规则是个人还是 Team。
3. 换一个出口，或先在面板里选普通转发兜底。
4. 不要把“没有 state”当成网络错误反复重试。

### Codex 返回 415

请求体编码不是 gzip、zstd 或未压缩。确认 Codex 版本，并看面板日志里的编码错误。

### Codex 返回 426

请求使用了 WebSocket。当前只支持 HTTP/SSE。

### 面板打不开

看 `%LOCALAPPDATA%\GPTGuard\logs\` 最新日志。常见原因是端口 17842 被占用，或守护没有启动。改 `config.json` 的 `ui.web_port` 后重启守护。

### 停止后仍然无法联网

守护被强制结束时，hosts 段和防火墙规则可能还在。用管理员终端运行：

```powershell
gptguard set network-guard false
gptguard run
```

等一轮检测通过，或手动删除 hosts 里的 GPTGuard 段和 `GPTGuard Block *` 防火墙规则。

## 卸载

```powershell
gptguard autostart off
gptguard update-lock off
gptguard codex restore
```

停止守护和托盘，然后删除：

```text
%APPDATA%\GPTGuard
%LOCALAPPDATA%\GPTGuard
```

Codex 原配置由 `codex restore` 恢复。备份目录里仍有历史副本，可自行保留。
