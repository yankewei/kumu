# Kumu

在一个 macOS 窗口中管理本地编码 Agent 的项目和终端。

Kumu 将 Codex、Claude Code、OpenCode、Pi、omp 等工具的原生终端界面放进同一个工作区，方便在不同项目和任务之间切换。

## 功能

- **项目管理**：每个项目拥有独立的终端布局，切换项目时后台终端继续运行；项目可重命名或删除，删除会关闭其全部终端。
- **分栏终端**：向右或向下拆分面板，同时处理多个任务；`⌘D` 向右分屏、`⌘⇧D` 向下分屏，`⌘⌥←/→/↑/↓` 在窗格间移动焦点。
- **工作区恢复**：重新打开应用时恢复上次的分屏布局与比例、选中的项目、焦点终端和窗口位置。
- **原生 Agent 体验**：直接运行 Agent 自带的交互界面，保留熟悉的命令和操作方式。
- **本地数据**：项目与终端布局保存在本机 SQLite 数据库中。
- **Agent 检测**：在设置中查看已安装的 Agent，支持深浅色外观。
- **Agent 通知**：在设置中为 Codex、Claude Code、OpenCode、Pi 和 Oh My Pi 安装持久通知接入，接收任务结束、提问与确认提醒；后台终端所属项目显示未读标记，应用在后台时发送 macOS 系统通知，点击可回到对应终端，并可选择是否播放提示音。可在 **设置 → 通用 → Agent 通知** 中关闭。
- **应用内更新**：启动时检查新版本，在侧栏查看更新提示、下载进度和重启入口。

## 安装

目前提供 **Apple Silicon Mac** 安装包。

1. 前往 [Releases](https://github.com/yankewei/kumu/releases/latest) 下载 `.dmg` 文件。
2. 打开 DMG，将 **Kumu.app** 拖入 **Applications**。
3. 从应用程序目录启动 Kumu。

使用安装包无需安装 Rust 或 Zig。编码 Agent 需要自行安装，并按各自的要求完成登录或配置。

## 开始使用

1. 添加一个本地项目目录。
2. 选择项目，在项目目录下打开终端；点击项目旁的新建终端按钮可增加窗格。
3. 输入 `codex`、`claude`、`opencode`、`pi` 或 `omp`，启动要使用的 Agent。

需要同时处理多个任务时，可拆分终端面板。关闭最后一个终端后项目会保留，可随时新建终端。打开 **设置 → 编码 Agent**，可以查看 Agent 的安装检测结果。

打开 **设置 → 服务商 → 编码 Agent**，对已安装的 Agent 点击 **开启通知**，再重新启动该 Agent。通知 hooks／扩展会注册到 Agent 自己的配置或自动发现目录；随后可照常通过命令、绝对路径或自定义脚本启动，Kumu 原样保留启动参数。已开启时同一位置显示 **关闭通知**，未安装的 Agent 只显示“未安装”，不提供通知操作。Codex 需要在 `/hooks` 中审查并信任新增 hooks。设置只管理 Kumu 自己的注册，保留已有 hooks 与配置注释。详见 [Agent 通知接入说明](docs/agent-notifications.md)。

macOS 系统通知需要从 `.app` 启动，并在首次提醒时允许通知；也可在 **系统设置 → 通知 → Kumu** 中调整。`cargo run` 提供应用内提醒与未读标记。

可在终端中运行以下命令测试，然后切换到其他终端、项目或应用：

```sh
sleep 3; printf '\033]9;任务完成\007'
```

项目与布局数据库位于 `~/.kumu/kumu.db`。

## 从源码运行

需要 macOS、Rust、Zig 0.17 和 Xcode。

```bash
git clone --recurse-submodules https://github.com/yankewei/kumu.git
cd kumu
cargo run
```

如果克隆时没有初始化子模块，先运行：

```bash
git submodule update --init --recursive
```
