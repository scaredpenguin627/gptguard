# GPTGuard

GPTGuard 是一个 Windows 出口保护工具。它把 Codex 的请求固定到经过检测的本机出口。出口失效、节点退出或配置异常时，GPTGuard 停止转发，不会自动改走直连。

## 小白怎么用

Release 页面只需要下载一个文件：`GPTGuard-Setup-v0.4.2-windows-x64.exe`。

1. 双击安装器。
2. 启动 GPTGuard，等待它检测 Codex 和本机代理。
3. 如果已经运行代理，GPTGuard 会尝试自动使用它。
4. 如果没有检测到代理，在窗口里粘贴订阅地址，或选择 YAML、URI、Base64 节点文件。
5. 点击“立即检测”。检测通过后，查看 Codex 配置差异并点击“确认接管配置”。
6. 点击“开启严格保护”。以后 GPTGuard 会在通知区运行。

## 协议节点

安装器已经内置 Mihomo。GPTGuard 会生成仅出站配置，关闭 TUN、DNS 和外部控制 API，并监督独立的 Mihomo 子进程。

支持的节点类型包括 Shadowsocks、VMess、VLESS、Trojan、Hysteria、Hysteria2、TUIC、HTTP 和 SOCKS5。Mihomo 版权和许可证见 `THIRD_PARTY_NOTICES.md`。

节点导入支持：

- 订阅地址。
- Clash/Mihomo YAML。
- URI 列表。
- Base64 URI 订阅。

导入后，GPTGuard 会自动关闭直连候选。没有可用节点时，网关返回 503。

## 主界面

主界面只提供以下操作：

- 开始保护。
- 立即检测。
- 添加订阅。
- 修复 Codex。
- 恢复原配置。
- 查看诊断。

关闭窗口只会隐藏到通知区。托盘菜单中的“退出 GPTGuard”会停止后台；需要撤销接管时，先点击“恢复原配置”。

## 高级与开发者

普通用户不需要命令行。开发者和排障人员可以使用同一个 EXE 的隐藏 CLI：

```powershell
GPTGuard.exe --cli status
GPTGuard.exe --cli doctor
GPTGuard.exe --cli codex status
GPTGuard.exe --cli codex restore
```

旧的 `web` 命令、管理 token、`gptguard-cli.exe`、CMD 启动脚本和单独的 `mihomo.exe` 不再属于 v0.4.0 的安装路径。

## 从源码构建

普通开发构建不包含 Mihomo 二进制，因此启用协议节点时会提示使用 Release 构建。发布构建先下载并校验锁定版本：

```powershell
$env:GPTGUARD_MIHOMO_PATH = (./tools/fetch_mihomo.ps1)
cargo build --release
```

Windows x64 发布使用 `installer/GPTGuard.iss`。发布工作流只上传一个安装 EXE，不上传 ZIP、7z、独立内核或 CLI。

## 测试

没有 Visual Studio 链接器时，可以使用仓库已有的 GNU 工具链：

```powershell
rustup run stable-x86_64-pc-windows-gnu cargo test --all-targets
```

真实 Mihomo 进程测试需要在 Windows 环境提供 `GPTGUARD_TEST_MIHOMO`。常规单元测试使用本地 mock，不会调用真实账号。

## 数据位置

- 配置：`%APPDATA%\GPTGuard\config.json`。
- 日志和状态：`%LOCALAPPDATA%\GPTGuard\`。
- Mihomo 运行时：`%LOCALAPPDATA%\GPTGuard\runtime\`。
- Codex 配置：`%USERPROFILE%\.codex\config.toml`，或 `CODEX_HOME` 指定的位置。

## 许可证

GPTGuard 使用 GPL-3.0-only。第三方组件和 Mihomo 的版权信息见 `THIRD_PARTY_NOTICES.md`。
