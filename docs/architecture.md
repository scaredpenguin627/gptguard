# GPTGuard 架构

## 运行结构

```text
GPTGuard.exe
├── Slint 原生窗口与通知区
├── Tokio 出口检测和 Codex 网关
├── Codex 配置事务与 provider 迁移
└── Mihomo 监督器
    └── %LOCALAPPDATA%\GPTGuard\runtime\<hash>\mihomo.exe
```

GPTGuard 是 Rust 主进程。Mihomo 仍作为独立子进程运行，所以 Go 内核不会进入 Rust 地址空间。内核文件由 Release 构建嵌入 EXE，首次启动时按哈希释放到当前用户目录。

## 网络边界

- Codex 网关只绑定 `127.0.0.1`。
- Mihomo 的每个混合入口只绑定 `127.0.0.1`。
- Mihomo 配置关闭 TUN、DNS、外部控制器和 LAN 访问。
- 节点退出、配置校验失败或出口检测失败时，网关返回 503。
- 严格保护不补直连出口。

## 配置事务

接管 Codex 前，GPTGuard 保存完整原文件、原始哈希、安装后哈希和 provider ID。

内置 `openai` provider 保持 `model_provider = "openai"`，只写入 `openai_base_url`。自定义 provider 保持原 ID，只临时替换自己的 `base_url`。旧版 `gptguard` provider 优先从事务备份恢复；当前文件变化时，先保存冲突副本。

## 用户界面

主界面由 Slint 编译到 Rust。它通过本机管理 API 调用后台状态，但不打开浏览器，也不把管理端口展示给用户。旧管理 API 仅作为隐藏 CLI 和迁移兼容层保留。

## 发布

Windows x64 发布由 GitHub Actions 完成：

1. 下载 `tools/mihomo.lock.json` 指定的官方 Mihomo 压缩包。
2. 校验 SHA-256 并提取 `mihomo.exe`。
3. 设置 `GPTGUARD_MIHOMO_PATH`，构建 GPTGuard。
4. 使用 `installer/GPTGuard.iss` 生成单文件安装器。
5. 检查发布目录只包含一个产品 EXE。
