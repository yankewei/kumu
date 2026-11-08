# Kumu

在一个 macOS 窗口中管理本地编码 Agent 的项目、会话和终端。

Kumu 将 Codex、Claude Code、OpenCode、Pi、omp 等工具的原生终端界面放进同一个工作区，方便在不同项目和任务之间切换。

## 功能

- **项目与会话管理**：按项目组织会话，集中查看正在进行的任务。
- **分栏终端**：向右或向下拆分面板，同时处理多个任务。
- **原生 Agent 体验**：直接运行 Agent 自带的交互界面，保留熟悉的命令和操作方式。
- **本地数据**：项目与会话记录保存在本机 SQLite 数据库中。
- **Agent 检测**：在设置中查看已安装的 Agent，支持深浅色外观。
- **应用内更新**：启动时检查新版本，在侧栏查看更新提示、下载进度和重启入口。

## 安装

目前提供 **Apple Silicon Mac** 安装包。

1. 前往 [Releases](https://github.com/yankewei/kumu/releases/latest) 下载 `.dmg` 文件。
2. 打开 DMG，将 **Kumu.app** 拖入 **Applications**。
3. 从应用程序目录启动 Kumu。

使用安装包无需安装 Rust 或 Zig。编码 Agent 需要自行安装，并按各自的要求完成登录或配置。

## 开始使用

1. 添加一个本地项目目录。
2. 新建会话，在项目目录下打开终端。
3. 输入 `codex`、`claude`、`opencode`、`pi` 或 `omp`，启动要使用的 Agent。

需要同时处理多个任务时，可拆分终端面板。打开 **设置 → 编码 Agent**，可以查看 Agent 的安装检测结果。

项目与会话数据库位于 `~/.kumu/kumu.db`。

## 从源码运行

需要 macOS、Rust、Zig 0.16 和 Xcode。

```bash
git clone --recurse-submodules https://github.com/yankewei/kumu.git
cd kumu
cargo run
```

如果克隆时没有初始化子模块，先运行：

```bash
git submodule update --init --recursive
```
