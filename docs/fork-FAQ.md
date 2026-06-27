# FAQ — Zed Perforce Fork

## Release Channel 与安装

### Q: 安装 preview/dev 版会覆盖我的 stable Zed 吗？

**不会。** Inno Setup 按 channel 隔离安装目录：

| Channel | 安装目录 | AppId (Windows) | 单实例 Mutex |
|---|---|---|---|
| Stable | `C:\Program Files\Zed` | `{{2DB0DA96-CA55-49BB-AF4F-64AF36A86712}` | `Zed-Stable-Instance-Mutex` |
| Preview | `C:\Program Files\Zed Preview` | `{{F70E4811-D0E2-4D88-AC99-D63752799F95}` | `Zed-Preview-Instance-Mutex` |
| Nightly | `C:\Program Files\Zed Nightly` | `{{1BDB21D3-14E7-433C-843C-9C97382B2FE0}` | `Zed-Nightly-Instance-Mutex` |
| Dev | `C:\Program Files\Zed Dev` | `{{8357632E-24A4-4F32-BA97-E575B4D1FE5D}` | `Zed-Dev-Instance-Mutex` |

Windows 将它们视为**完全独立的程序**——独立安装、独立卸载、独立单实例检测。两个 channel 可以**同时运行**。

### Q: 设置、扩展、数据库共享吗？

**是的，完全共享。** 所有 channel 共用同一个 `APP_NAME = "Zed"`（`crates/paths/src/paths.rs` 第 18 行），路径不受 channel 影响：

| 类型 | Windows 路径 | 各 Channel 共享？ |
|---|---|---|
| 设置 | `%APPDATA%\Zed\settings.json` | ✅ |
| 快捷键 | `%APPDATA%\Zed\keymap.json` | ✅ |
| 扩展 | `%LOCALAPPDATA%\Zed\extensions` | ✅ |
| 数据库 | `%LOCALAPPDATA%\Zed\db` | ✅ |
| 日志 | `%LOCALAPPDATA%\Zed\logs` | ✅ |
| 主题 | `%LOCALAPPDATA%\Zed\themes` | ✅ |

这意味着两个 channel 同时运行时，会**并发读写同一个数据库**，存在数据损坏风险。

### Q: 有哪些潜在的冲突点？

安装向导里的可选 task 会影响两个 channel 的共存：

| Task | 说明 | 建议 |
|---|---|---|
| `associatewithfiles` | 后装的 channel 会抢走 stable 的文件关联（如 `.txt`、`.rs` 的默认打开程序） | 安装时取消勾选 |
| `addtopath` | 两个 `zed` CLI 命令都在 PATH 里，执行哪个取决于 PATH 顺序 | 安装时取消勾选 |
| `addcontextmenufiles` | 右键菜单会同时出现 "Zed" 和 "Zed Preview"，共存不冲突 | 按需勾选 |
| URL scheme `zed://` | 后装的 channel 会接管 URL handler | 注意 |

### Q: Dev channel 有什么特殊行为？

从 `crates/release_channel/src/lib.rs` 和 `crates/zed/src/main.rs`：

| 行为 | Dev | Stable/Nightly/Preview |
|---|---|---|
| 单实例检查 | **跳过** — 可同时运行多个实例 | 强制单实例 |
| 崩溃处理器 | 默认不启用 | 默认启用 |
| 自动更新轮询 | 不轮询 | 轮询 |
| 崩溃时自动重启 | 不自动重启 | 自动重启 |

### Q: 为什么 fork 不改 `APP_NAME` 做完全隔离？

`paths.rs` 第 14-18 行的注释已经写了：

> Forks should change this to avoid colliding with Zed's user data.

但目前没有改，原因：

1. **方便对比测试** — 用同一套 settings/extensions 跑 fork，无需重新配置
2. **改动影响面大** — `APP_NAME` 是 const 级硬编码，改它会影响所有派生路径、crate 依赖
3. **暂不面向终端用户** — fork 当前是开发/验证阶段，不需要与 stable 完全隔离

如果未来 fork 进入面向用户阶段，应该将 `APP_NAME` 改为 `"Zed-P4"` 之类的独立名字。

## Fork 版本信息

### Q: 这个 fork 基于哪个 upstream 版本？

截至 2026-06-24：

| | 版本 | Commit | 日期 |
|---|---|---|---|
| **Fork 基点** | Zed 1.8.0 | `c3c38c5` — *git_ui: Add View File action to Git Panel (#59383)* | 2026-06-16 |
| **当前 fork HEAD** | Zed 1.8.2 | `57fa532779` — *Merge tag 'v1.8.2-pre'* | 2026-06-24 |
| **Upstream 最新** | Zed 1.9.0 | `c49a29f` — *sandbox: Linux domain filtering* | 2026-06-24 |

Fork 分支相对 upstream main 有 **48 个独立 commits**，upstream 在 fork 之后又前进了 **128 个 commits**。

## 打包构建

### Q: `cargo build --release` 产出安装包吗？

**不，只产出裸的 `zed.exe`。** 要生成 Windows 安装包需要运行：

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File script/bundle-windows.ps1
```

### Q: bundle-windows.ps1 的前置依赖？

| 依赖 | 说明 |
|---|---|
| **PowerShell 7+** (`pwsh`) | 脚本使用了 PS7 的三元运算符 `? :`，Windows PowerShell 5.1 不兼容。安装：`winget install Microsoft.PowerShell` |
| **Inno Setup 6** | 安装包编译器，默认路径 `C:\Program Files (x86)\Inno Setup 6\ISCC.exe` |
| **Visual Studio 2022** | 脚本调用 `Launch-VsDevShell.ps1` 初始化 MSVC 编译环境 |
| **Windows SDK 10.0.26100.0** | `makeAppx.exe` 用于生成 Explorer 右键菜单的 Appx 包 |
| **cargo-about** | license 生成工具。首次 bundle 会自动安装，但如果 agent terminal 的 `\\?\` 前缀 TMP 导致编译失败，需手动预装 |

### Q: bundle 脚本会编译哪些 package？

不只是 `zed`，还有四个附属程序：

| Package | 产物 | 说明 |
|---|---|---|
| `zed` | `Zed.exe` | 主编辑器 |
| `cli` | `bin\zed.exe` | CLI 入口（`zed` 命令） |
| `auto_update_helper` | `tools\auto_update_helper.exe` | 自动更新辅助 |
| `explorer_command_injector` | `zed_explorer_command_injector.dll` | 资源管理器右键菜单集成 |
| `remote_server` | `zed-remote-server-windows-x86_64.zip` | 远程服务器 |

此外还下载：
- AMD GPU Services SDK（`amd_ags_x64.dll`）
- ConPTY（`OpenConsole.exe` + `conpty.dll`）
