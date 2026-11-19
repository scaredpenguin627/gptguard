# Windows 使用说明

## 普通用户

1. 从 Release 下载 `GPTGuard-Setup-v0.4.5-windows-x64.exe`，双击安装。
2. 在 Clash Verge 选好节点，右键 GPTGuard 并选择“以管理员身份运行”。
3. 点击“开启严格保护”。首次使用时，GPTGuard 自动查找本机代理并检测出口 IP。
4. 核对 IP 后点击“信任并开启保护”。如果复测时 IP 发生变化，选择稳定的节点，再点击“开启严格保护”。
5. 在“Codex 网关”查看改动，点击“确认接管配置”。看到“已保护”后可以关闭窗口，后台会留在通知区运行。

安装器不要求用户预先安装 Rust、Mihomo、PowerShell 脚本或浏览器面板。

## 首次启动没有代理

展开“其他连接方式”，点击“选择文件”导入 YAML、URI 或 Base64 节点文件，也可以粘贴订阅地址。将已知可信的出口 IP 或 CIDR 网段加入白名单，然后点击“开启严格保护”。GPTGuard 会解析节点并生成自己的 Mihomo 配置。

GPTGuard 只把 Mihomo 和 Codex 网关绑定到 `127.0.0.1`。端口由程序管理，用户无需填写。

## provider mismatch

旧版本可能把 `model_provider` 写成 `gptguard`。退出 Codex 后启动 v0.4.0，在窗口中点击“确认接管配置”。程序会优先从事务备份恢复原 provider 身份，再把本地网关地址写入 endpoint。当前文件被手动修改时，程序会先保存冲突备份并停止自动覆盖。

## 严格保护

严格保护开启后，以下情况都会停止 Codex 转发：

- 出口检测失败。
- Mihomo 节点全部失效。
- Mihomo 配置校验失败。
- Codex 接管事务校验失败。
- 本地网关或运行时组件异常。

GPTGuard 不会在这些情况下自动选择直连。

## 高级命令

普通用户不需要命令行。开发者可以从安装目录运行：

```powershell
GPTGuard.exe --cli status
GPTGuard.exe --cli doctor
GPTGuard.exe --cli codex status
GPTGuard.exe --cli codex restore
```

`web` 命令、旧的 `gptguard-cli.exe` 和 CMD 启动脚本只用于旧版本兼容，不属于 v0.4.0 的主路径。

## 排障

点击“查看诊断”可以核对后台的管理员权限。以管理员身份重新启动时，GPTGuard 会接替尚未开启保护的普通权限后台。如果窗口提示后台版本较旧，或仍提示权限不足，先关闭主窗口，再从 Windows 通知区选择“退出 GPTGuard”，然后右键启动新版。

日志在 `%LOCALAPPDATA%\GPTGuard\logs\`。先在主界面点击“查看诊断”，再检查：

- Codex 配置是否存在。
- 旧事务是否有完整备份。
- 节点文件是否包含受支持的协议。
- Windows 防火墙或安全软件是否阻止了 GPTGuard 或内置 Mihomo。

卸载前，GPTGuard 会尝试恢复 Codex 原配置。如果当前文件已经被手动改过，程序会保留现场并要求进入高级恢复。
