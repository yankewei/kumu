# Artifacts：终端 Agent 的可视化产物

> 状态：P0 + P1 已实现（存储、CLI、control 通知、Artifacts 抽屉、Markdown 阅读页、`--replace`、内嵌 HTML WebView）；P2 打磨待做
> 分支：`feat/artifact-rendering`

## 1. 问题与目标

Claude Code、OpenCode、Antigravity 都在终端里运行，但它们产出的方案、设计稿、报告常常是 Markdown 或 HTML。终端只能显示文本，HTML 里的布局、样式和交互完全丢失。

Kumu 要提供一条通用链路：

1. Agent 在需要交付可视化产物时，调用一个 CLI 工具发布 artifact；
2. artifact 以快照形式写入项目的 `.kumu/artifacts/<id>/`，随项目走，不依赖数据库；
3. 发布时自动记录「哪个 pane / 哪个 Agent / 哪个会话」；
4. Kumu 右侧 Artifacts 抽屉按会话分组展示整个项目的 artifacts，任意会话都能打开；
5. Markdown 用原生 `TextView` 渲染，HTML 用内嵌 `WKWebView` 渲染；
6. Agent 不在 Kumu 里运行时也能发布，只是会话信息退化为「未关联」。

非目标（本阶段不做）：

- 云端同步、分享链接、多人协作；
- 任意语言/框架的构建执行（artifact 是静态产物，不在 Kumu 里跑 dev server）；
- 替代 Agent 自己的会话历史。

## 2. 总体架构

```
┌─────────────────────────────── Kumu.app ───────────────────────────────┐
│  Home (GPUI)                                                           │
│   ├─ Term / panes ── KUMU_PANE / KUMU_APP_BINARY / KUMU_SESSION_TOKEN  │
│   ├─ Control socket ◄── artifact publish / open / refresh              │
│   └─ Artifacts drawer + reading sheet                                  │
│         ├─ Markdown  → nsh TextView                                    │
│         └─ HTML      → WKWebView (GPUI native composition surface)     │
└────────────────────────────────────────────────────────────────────────┘
                ▲                                   │
                │ control notify                    │ 读写
┌───────────────┴──────────────┐        ┌───────────▼────────────────────┐
│ Agent (Claude/OpenCode/agy)  │        │ <project>/.kumu/artifacts/<id>/ │
│   └─ kumu artifact publish   │        │   artifact.json                │
└──────────────────────────────┘        │   index.md | index.html | ...  │
                                        └────────────────────────────────┘
```

职责边界：

- **CLI 是唯一写入方**。UI 只读磁盘，不参与 artifact 内容写入，避免并发与大文件穿过 control socket（socket 单请求上限 256 KB）。
- **磁盘是唯一事实来源**。不新增 artifact 数据表；UI 扫描 + 内存缓存即可，数据库损坏或重装不影响产物。
- **control socket 只做通知与打开**。发布后刷新抽屉；`--open` 时打开阅读页。

## 3. CLI

命令挂在现有 `kumu` 可执行文件上，`src/control/cli.rs` 在启动应用前分发。

### 3.1 发布

```sh
# 在 Kumu 窗格里
"$KUMU_APP_BINARY" artifact publish ./plan.md --title "登录页重构方案"
"$KUMU_APP_BINARY" artifact publish ./mockup.html --title "交互原型" --open

# 从 stdin 发布
cat <<'EOF' | "$KUMU_APP_BINARY" artifact publish - --title "排查报告"
# 结论
...
EOF

# 覆盖已有 artifact（同名迭代，不产生新条目）
"$KUMU_APP_BINARY" artifact publish ./mockup.html --replace 20261128-153012-a1b2
```

参数：

| 参数 | 说明 |
| --- | --- |
| `FILE` | 文件或目录；`-` 表示 stdin |
| `--title T` | 展示标题；缺省取首个 Markdown 标题 / HTML `<title>` / 文件名 |
| `--kind auto\|markdown\|html\|text` | 缺省按扩展名推断 |
| `--entry FILE` | 发布目录时指定入口文件；缺省找 `index.html` / `index.md` |
| `--open` | 发布后在 Kumu 打开阅读页 |
| `--no-notify` | 不通知 Kumu（仅落盘） |
| `--replace ID` | 原子替换已有 artifact 的内容，保留 id / 创建时间 / 会话 |
| `--project DIR` | 不在 Kumu 内时显式指定项目根目录 |
| `--agent ID` / `--session ID` | 不在 Kumu 内时手动补会话信息 |
| `--tag T`（可重复） | 预留的筛选标签 |
| `--json` | 输出 `{ id, title, kind, entry, dir, session }` |

输出示例：

```
artifact 20261128-153012-a1b2  plan.md  markdown
/Users/me/repo/.kumu/artifacts/20261128-153012-a1b2/index.md
```

### 3.2 其它命令

```sh
kumu artifact list [--json] [--session KEY] [--limit N]
kumu artifact path <ID>          # 打印入口文件绝对路径
kumu artifact open <ID|PATH> [--project DIR]  # 让 Kumu 打开阅读页
kumu artifact remove <ID> [--force]  # 移入废纸篓；非 TTY 时必须带 --force
```

`open` 在 Kumu 外调用时没有 `KUMU_PANE`，因此请求必须带 `projectPath`：Kumu 按规范化路径匹配**已打开的项目**，若该项目在项目列表里但未选中，先切换过去再打开阅读页；项目不在 Kumu 中时报 `projectNotOpen`，提示用户先把目录加入 Kumu（不静默创建项目）。

### 3.3 项目与会话解析

**在 Kumu 窗格内**（`KUMU_PANE` 存在）：

1. 调 `kumu panes --json`，用 `KUMU_PANE` 找到自己；
2. 从 `PaneSummary` 读取 `projectPath`、`agent`、`agentSessionId`、`title`、`tabName`（本次新增字段，均为增量 JSON 字段）；
3. 写入 artifact.json；
4. 调 `artifact published` / `artifact open` 通知 UI，同时带上 `projectPath`。

`PaneSummary` 的取值要覆盖两种情况：

- **Kumu 直接启动的 Agent pane**：来自 `launches`；
- **在普通 shell 里手动启动、被 Kumu 接管的 Agent**：`pane_is_shell` 仍是 `true`，但 `agent_status` / `launches` 已有 Agent 与会话信息。`control_summary` 需要按 `plugin_panes` 的同一套回退逻辑取 `agent`，标题优先取 `AgentEntry::display_title()`，`agentSessionId` 取 `launches` 里的 `session`。

`agent` 字段仅用于展示与 artifact 归属，`pane send` 的安全性仍由 `control_deliver` 独立校验 `pane_is_shell == false`，不受影响。

**不在 Kumu 内**：

1. `--project` 优先；
2. 否则从 cwd 向上找最近的 `.kumu/`，再找 `.git`（文件或目录均可，兼容 worktree）；
3. 都没有则用 cwd，并在 stderr 提示「建议在项目根目录发布，或用 --project 指定」；
4. `agent` / `agentSessionId` 取自 `--agent` / `--session`，否则为 `null`（UI 归入「未关联」）。

`--kind auto` 遇到无扩展名（典型是 stdin `-`）时：内容以 `<!doctype` / `<html` 开头（忽略大小写与前导空白）判为 `html`，否则 `markdown`。标题缺省依次取：首个 Markdown 标题 / HTML `<title>` / 文件名；stdin 且都没有时用 `Untitled`。

### 3.4 Agent 接入方式

- 统一 CLI 对 Kumu 支持的全部 Agent 通用（`src/agent/registry.rs` 中的 8 个：Claude Code、Codex、OpenCode、Antigravity、Cursor、Pi、Oh My Pi、Grok Build），不分别写插件；
- 新增 skill `skills/kumu-artifacts/SKILL.md`，通过既有的 `npx skills add yankewei/kumu` 安装；
- skill 说明何时发布（方案、设计稿、架构图、报告、可交互原型）、如何选 Markdown/HTML、如何避免把大段 HTML 打到终端、`--open` 的使用时机；
- P2 可再加一个薄 MCP server 包装同一套 CLI，供偏好 MCP 的客户端使用，不影响主链路。

## 4. 存储格式

```
<project>/.kumu/
  .gitignore                 # 内容：artifacts/   （快照默认不进 git）
  artifacts/
    20261128-153012-a1b2/
      artifact.json
      index.md
    20261128-154501-7c3e/
      artifact.json
      index.html
      assets/...
```

`artifact.json`（v1）：

```json
{
  "version": 1,
  "id": "20261128-153012-a1b2",
  "title": "登录页重构方案",
  "kind": "markdown",
  "entry": "index.md",
  "createdAt": "2026-11-28T07:30:12Z",
  "updatedAt": "2026-11-28T07:30:12Z",
  "source": "/Users/me/repo/docs/plan.md",
  "tags": [],
  "session": {
    "key": "agent-session:5f0c...",
    "pane": 7,
    "agent": "claude",
    "agentSessionId": "5f0c...",
    "title": "登录页重构",
    "tabName": "登录页",
    "cwd": "/Users/me/repo"
  }
}
```

约定：

- **id**：`YYYYMMDD-HHMMSS-<4 位随机 hex>`（UTC，可排序）；碰撞时追加 `-2`。
- **kind**：`markdown` / `html` / `text`；`text` 用等宽只读视图呈现。
- **entry 永远是 artifact 目录内的相对路径**，UI 读取前做 canonicalize + 前缀校验，拒绝越界。
- **快照语义**：默认复制内容；HTML 的兄弟资源通过目录发布（`--entry`）或自包含单文件。`source` 只作溯源，不回读。
- **目录发布安全**：默认**拒绝复制 symlink**（stderr 列出并跳过），避免把 `~/.ssh` 等目录外内容带进 artifact；如确需跟随，显式 `--follow-symlinks`，且目标必须位于源目录内。单 artifact 上限：总大小 64 MB、文件数 500，超出即拒绝。
- **原子写**：先写 `<id>.tmp-<pid>/`，`fsync` 文件，再 `rename` 为最终目录。
- **`--replace` 原子替换**：POSIX 的 `rename(tmp, existing_nonempty_dir)` 会返回 `ENOTEMPTY`，不能单步覆盖。macOS 用 `renameatx_np(RENAME_SWAP)` 做原子交换；失败时退化为两步：`rename(old, old.bak-<pid>)` → `rename(tmp, old)` → 删除备份，第二步失败则回滚。扫描时忽略 `*.tmp-*` 与 `*.bak-*`。
- **删除**：UI 默认移入废纸篓（`NSFileManager.trashItemAtURL`），暂不提供永久删除。CLI 在非 TTY 且没有 `--force` 时直接退出并提示，避免在 Agent 子进程里等待交互确认。
- **容量**：单文件超过 32 MB 拒绝发布；`list` 时忽略无 `artifact.json` 的目录。

## 5. Session 关联与分组

### 5.1 关联字段

- `agentSessionId`：Kumu 通过 `kumu-session:` 收到并用于恢复的会话 id，是跨重启、跨窗格恢复的主键；
- `pane`：发布时的窗格 id，仅作即时定位；
- `key` 的生成优先级：
  1. `agent-session:<agentSessionId>`；
  2. `pane:<paneId>:<agent>`（Agent 尚未上报会话 id 时）；
  3. `unattached`。

### 5.2 分组展示

- 抽屉列出**整个项目**的 artifacts（项目内共享），默认按 session 分组，组内按 `updatedAt` 倒序；
- 组标题：Agent logo + `session.title`（发布时快照）或 `Agent 名 · 窗格 N` + tab 名；
- 顶部筛选：`全部` / `当前会话`（用当前 pane 的 session key 匹配）；
- 同一会话被恢复后，新 pane 仍归入同一组；
- 当前没有活跃 pane 的历史会话组照常显示，标注「已结束」；
- 分组排序：**当前会话 → 其它活跃会话 → 已结束会话 → 未关联**，兼顾全局可见性与当前上下文效率。

### 5.3 何时补写

CLI 发布时写入当时的会话快照。Agent 会话 id 在发布之后才上报时，该 artifact 仍归在 pane 组，不做事后迁移；这比在 UI 里猜测归属更可预测。

## 6. Control 协议

在 `src/control/protocol.rs` 增加两个请求（均为通知型，回复轻量）：

```jsonc
// CLI → App：发布完成，刷新/标记未读
{
  "op": "artifactPublished",
  "from": 7,                                  // 可选：在 Kumu 窗格内时提供
  "projectPath": "/Users/me/repo",             // 必填：规范化后的项目根
  "artifact": "20261128-153012-a1b2"
}

// CLI → App：打开阅读页（--open 或 artifact open）
{
  "op": "artifactOpen",
  "from": 7,                                  // 可选
  "projectPath": "/Users/me/repo",
  "artifact": "20261128-153012-a1b2"
}
```

回复：`{ "ok": true, "opened": true }`。

处理逻辑（`src/ui/home/control_handler.rs`）：

- `from` 变成可选：有则校验 pane 存在并走 `paneGone`；没有则完全依赖 `projectPath`，覆盖普通终端里 `kumu artifact open` 的场景；
- 项目按 `projectPath` 规范化匹配 Kumu 的项目列表，不依赖 cwd；
- `artifactPublished`：只标记该项目的未读状态；抽屉已打开且正显示该项目时，直接通知已挂载的 plugin 重新扫描；
- `artifactOpen`：若项目在列表中但未选中，先切换项目，再打开阅读页并激活窗口；项目不在列表中返回 `projectNotOpen`；
- `from` 只用于「当前会话」高亮与校验来源窗格，不参与项目解析；
- 无 Kumu 运行时 CLI 直接跳过通知，不报错。

**窗口句柄**：控制循环改为 `cx.spawn_in(window, ...)`，每条请求通过 `this.update_in(cx, ...)` 交给 `handle_control_command(command, window, cx)`，因此打开阅读页时直接拿到 `&mut Window`，不需要在 `KumuApp` 上缓存 `AnyWindowHandle`，也不会重入同一个 entity。阅读页本身是独立 GPUI view，不要求 Artifacts 抽屉已经创建。

**未读状态**：插件视图在抽屉关闭后会被销毁（`plugins_host.rs` 的 `plugin_views.clear()`），所以未读标记必须存在 `KumuApp` 上，按 `project_id` 维护，例如 `artifact_unread: HashSet<i64>`；打开抽屉时清除该项目。P0 就实现这一点，与标题栏按钮的小圆点联动，不放到 P2。

`PaneSummary` 增量字段：

```json
{
  "projectPath": "/Users/me/repo",
  "agentSessionId": "5f0c...",
  "tabName": "登录页"
}
```

均为可选字段，旧调用方不受影响。`agent` / `title` 按 3.3 的回退逻辑补齐被接管 Agent 的信息。

## 7. UI 与渲染

### 7.1 右侧抽屉（Artifacts plugin）

- 复用现有 plugin 机制：`src/plugins/artifacts/`，`PluginSpec { id: "artifacts", title: "Artifacts" }`；
- 标题栏新增按钮，宽 300px（与 Git 面板一致，可拖拽 240–480）；
- 结构：搜索框 → 会话分组 → artifact 行；
- 行内操作：打开、在 Finder 中显示、复制路径、删除（确认）、重新加载；
- 刷新时机：面板打开、项目切换、`artifactPublished`、面板可见时 2 秒 mtime 轮询；
- 空状态直接给出可复制的发布命令。

### 7.2 全屏阅读页（sheet）

复用现有 `ActiveSheet`，结构对齐 Diff review：

```
┌ header: 返回/关闭 · 标题 · 会话 · 类型 · [在浏览器打开] [复制路径] [重新加载] ┐
├ body                                                                    ┤
│   Markdown: NSH TextView（可选择、代码高亮、链接）                       │
│   HTML:     WKWebView（铺满 body，内部滚动）                             │
└ footer: 发布时间 · 来源路径 · 大小                                       ┘
```

- 默认宽度 1040px，`max-w-[94%]`；
- `⌘+` / `⌘-` / `⌘0` 缩放 HTML；Markdown 跟随主题字号；
- `Esc` / 关闭按钮退出；webview 获得焦点时，应用的菜单快捷键仍优先（见风险）；
- 重新加载按钮重新读取磁盘，支持 Agent 用 `--replace` 迭代后原地刷新；
- P2：对 `--replace` 的 id 做文件监听，自动热更新。

### 7.3 Markdown 渲染

- 用 `nsh_base::TextView`，`markdown` 属性直接传入文件内容；
- 主题跟随 Kumu 亮/暗（沿用 `nsh_ui::theme::nsh_theme(appearance())` 的挂载方式）；
- 代码块用已启用的 `highlight` feature；表格、任务列表、脚注、alert 由 TextView 原生支持；
- 链接：`TextViewLinks` 默认是 `Report`，不依赖 NSH 的 `openUrl` 路由。通过 `on_link_press` 显式接管：
  - `http` / `https` / `mailto` 走 `cx.open_url`（与终端链接一致）；
  - 以 `#` 开头的是文内锚点，交给 TextView 自己滚动，不外部打开；
  - 本地相对路径基于 artifact 目录解析，绝对路径基于项目根校验，越界或不存在时提示而不打开。

### 7.4 HTML 渲染（内嵌 WebView）

最终实现没有手写 `objc2-web-kit` 封装，而是使用 `nsh-platform-gpui` 自带的 web view
（`webview` feature，底层 wry + WKWebView）。它已经处理 native surface、遮挡、输入、
下载、compositor 重建和 macOS 非持久 data store，Kumu 只需要提供 URL 和导航策略。

1. **loopback server**：打开 HTML artifact 时，为 artifact 目录启动一个只监听
   `127.0.0.1` 的临时 HTTP server（`src/artifacts/serve.rs`）：
   - 读取入口和所有相对资源（CSS/JS/字体/图片），也支持根路径 `/assets/...`；
   - 每个请求做百分号解码、路径组件校验、`canonicalize` 和 artifact 目录前缀校验，
     拒绝 `..`、绝对路径和 symlink 逃逸；
   - 只提供 `GET` / `HEAD`，按扩展名返回 MIME；
   - sheet 关闭时 `Drop` 停止 accept 线程。
2. **阅读页**：用 `web_view(entry_url)` 加载 `http://127.0.0.1:<port>/<entry>`。
   选择 HTTP 而不是 `file://` 的原因：
   - wry 的 URL 加载走 `loadRequest`，不会自动获得 `loadFileURL` 的目录读权限；
   - `web_view_html` 的 base URL 在 macOS 后端被忽略，相对资源无法解析；
   - NSH 的导航白名单只允许 `http` / `https`。
   loopback server 让相对路径、ES module、`fetch` 和根路径资源按浏览器语义工作。
3. **数据隔离**：`incognito(true)`，每个阅读页使用非持久 `WKWebsiteDataStore`，
   关闭后 localStorage / IndexedDB / Cookie 不保留。
4. **导航策略**：`on_navigation_state_change` 中，URL 留在当前 origin 内则放行；
   `http` / `https` 外链交给系统浏览器，然后用 `ElementRef::load` 重新加载 artifact；
   `about:blank` 等初始状态忽略。NSH 没有暴露 per-navigation decision 回调，
   因此采用「放行后立即回退」而不是拦截。
5. **重新加载**：`--replace` 或工具栏重新加载时，URL 追加 `?v=N`，绑定的 URL
   变化会触发 web view 重新加载；server 每次都从磁盘读文件。
6. **不可用**：`on_unavailable` 把消息写入 `web_error`，阅读页切换为
   「在浏览器打开」的 fallback。
7. **输入与层级**：native surface 由 NSH 平台层按 view box 放置，保持在 header /
   footer 之间；鼠标、滚动、文本选择和输入交给 WKWebView。

已通过真实应用冒烟验证：

- [x] `/index.html` 被本地 server 命中，`on_load_start` 与导航完成事件触发；
- [x] HTML + 相对 CSS/JS 正常按浏览器语义加载（`GET /index.html`）；
- [x] app 在 webview 创建后保持运行，无 panic；
- [ ] 外链点击、popover 遮挡、外接显示器切换仍需手工回归。

若某台机器上 WebKit 不可用，`web_view_available` / `on_unavailable` 会切到浏览器
fallback，不影响 Markdown 与 CLI 链路。

## 8. 代码落点

| 模块 | 内容 |
| --- | --- |
| `src/artifacts/mod.rs` | 领域模型、kind/标题推断、发布、`--replace`、扫描、项目根解析、Trash |
| `src/control/cli_artifact.rs` | `kumu artifact ...` 参数解析与输出 |
| `src/control/protocol.rs` | `ArtifactPublished` / `ArtifactOpen`、`PaneSummary` 新字段 |
| `src/control/server.rs` | 把 artifact 请求转发给 UI 线程 |
| `src/ui/home/mod.rs` | 按项目维护的 artifact 未读状态 |
| `src/ui/home/control_handler.rs` | 可选 `from` / `projectPath` 解析、切换项目、挂载阅读页、未读标记 |
| `src/ui/home/plugins_host.rs` | 被接管 Agent 的 `PaneTarget` / `control_summary` 回退、未读点 |
| `src/plugins/artifacts/mod.rs` | plugin 注册、抽屉快照、分组、刷新、打开阅读页 |
| `src/plugins/artifacts/view.rs` | NSH 抽屉列表（分组、搜索、筛选、删除确认） |
| `src/plugins/artifacts/sheet.rs` | 阅读页、NSH Markdown / HTML webview 分发、loopback URL |
| `src/artifacts/serve.rs` | artifact 目录的 loopback HTTP server（GET/HEAD、MIME、路径校验） |
| `skills/kumu-artifacts/SKILL.md` | Agent 使用说明 |
| `assets/icons/artifact.svg` 等 | 标题栏与行内操作图标 |
| `README.md` / `DESIGN.md` | 功能介绍与视觉规范补充 |

不新增数据库表；`SCHEMA_VERSION` 不变。

## 9. 实施顺序

### P0：链路、Markdown 与 `--replace`（可独立交付）

1. `artifacts` 模型/存储 + 单元测试（含 `--replace`、目录安全限制）；
2. `PaneSummary` 新字段、被接管 Agent 的回退、control 通知；
3. `kumu artifact publish/list/path/open/remove`；
4. Artifacts 抽屉 + 未读点 + Markdown 阅读页；
5. `--replace`（Agent 迭代高频，必须有；否则抽屉会被同名草稿刷屏）；
6. HTML 暂用「在浏览器打开」兜底；
7. skill 与 README。

### P1：内嵌 HTML

8. `nsh-platform-gpui` 开启 `webview` feature（wry + WKWebView）；
9. artifact loopback HTTP server（相对资源、路径校验、MIME）；
10. 阅读页 `web_view()` 接入、`incognito`、外链回退、`--replace` 重新加载；
11. 真实应用冒烟验证（页面请求、load 事件、无 panic）。

### P2：打磨

12. `--replace` 后自动热更新（文件监听）；
13. 搜索、标签筛选；
14. 图片/SVG artifact；
15. 可选 MCP 包装。

## 10. 测试计划

**单元测试（Rust）**

- id 生成格式、碰撞与排序；
- kind 推断（大小写、`.markdown`、无扩展名、stdin 的 HTML 嗅探）；
- 标题推断（Markdown H1 / HTML `<title>` / 文件名 / `Untitled`）；
- 项目根解析（`.kumu`、`.git` 文件、`--project`、cwd 兜底）；
- 发布原子性：临时目录清理、`--replace` 保留 id/createdAt、扫描忽略 `.tmp-*` / `.bak-*`；
- 目录发布：symlink 拒绝、总大小与文件数上限；
- 扫描：忽略损坏/无元数据目录、按 `updatedAt` 排序、越界 entry 拒绝；
- 会话 key 优先级；
- `remove` 在非 TTY 且无 `--force` 时直接失败；
- control 请求 serde round-trip（含可选 `from` / `projectPath`）。

**集成/手工**

- 在 Claude Code / OpenCode / Antigravity 三个 pane 里各发布 Markdown 与 HTML；
- 在普通 shell 里手动启动的 Agent 发布，会话分组仍正确；
- 同一会话连续发布、`--replace`、重启 Kumu 后分组不变；
- 普通终端（无 `KUMU_PANE`）发布 → 显示在「未关联」；`open` 能匹配已打开项目；
- HTML 加载 CDN 资源、本地资源、点击外链、`javascript:` / 未知 scheme 被拒绝；
- 亮/暗主题下的 Markdown 与 HTML；
- 删除进入废纸篓；Finder 定位；复制路径。

只保留真正覆盖边界行为的 case，避免为每种扩展名重复写测试。

## 11. 风险与决策

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| WebView 的 native surface 覆盖 GPUI 弹出层 | 打开阅读页时 ⌘K / dialog / sheet 内 popover 可能被遮住 | 已实现浏览器 fallback；P2 在 overlay 打开时临时隐藏 webview |
| loopback HTTP server 暴露 artifact 内容 | 本机其他进程可访问该临时端口 | 仅绑定 `127.0.0.1`、随机端口、sheet 生命周期内存在；路径 canonicalize + 前缀校验，拒绝 `..` / symlink 逃逸 |
| Agent 不遵循 skill，直接打印 HTML | 用户看不到产物 | skill 写清触发条件；CLI 错误信息带示例；README 提示 |
| 远程资源被墙/加载失败 | HTML 空白 | 页面保持原样；工具栏提供「在浏览器打开」 |
| `.kumu` 被误提交 | 仓库噪音 | 自动写 `.kumu/.gitignore` 忽略 `artifacts/` |
| 大文件/符号链接 | 磁盘占用、敏感文件泄漏 | 32 MB 单文件 + 64 MB 总量 + 500 文件；默认拒绝 symlink |

已确认的决策：

1. HTML 外链默认用系统浏览器打开，仅放行 `http/https`；Kumu 内不做第二阅读页。
2. `--replace` 进入 P0。
3. 抽屉默认筛选「全部」；分组排序为当前会话 → 其它活跃会话 → 已结束 → 未关联。
4. HTML 内嵌渲染使用 NSH web view + loopback server，而不是手写 WKWebView 封装。

## 12. 评审修订记录

针对首轮评审已修正：

- 控制循环改用 `spawn_in` + `update_in`，`handle_control_command` 直接收到 `&mut Window`；`from` 可选，增加 `projectPath`；
- `PaneSummary` 对 shell 内手动启动 Agent 的覆盖：复用 `agent_status` 回退与 `display_title()`；
- WKWebView 层级描述改为 gpui-fast 真实行为（native surface 在 overlay 之下），补充 popover 隐藏、数据存储隔离与 scheme 白名单；
- P1 最终采用 NSH `web_view()`（wry/WKWebView）+ loopback HTTP server，避免 `file://` 子资源读权限与导航白名单问题；
- 目录发布的 symlink 逃逸、总容量/文件数上限；
- `--replace` 的 POSIX 原子替换细节；
- Markdown 链接改为 `on_link_press` 显式接管；
- stdin kind 嗅探、非 TTY `remove`、按项目未读状态、Agent 范围措辞。

两处技术澄清（已写入正文）：

- `Host::Base` / `Host::Overlay` 在 gpui-fast 内部存在，但不是公开 API；可观察结论是 native surface 默认位于 overlay surface 之下，palette/dialog/toast 会盖住 webview，sheet 内 popover 需要主动隐藏 webview；
- `set_compositor_recreated_callback` 面向外部 GPU surface；`create_native_surface()` 创建的 GPUI 托管 surface 使用默认 no-op，改为用外接显示器/窗口重建做验收。
