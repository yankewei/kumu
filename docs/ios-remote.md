# iOS 远程控制设计（v1）

> 设计稿，不是实现说明。目标是把产品形态、协议和边界先定下来，再动代码。
>
> 客户端仓库：当前为 `kumu-mobile`（NSH 0.5，Rust-first）；服务端计划放在本仓库 `src/remote/`。

## 1. 目标

在 iPhone 上连接 Mac 上运行的 Kumu，直接操作正在跑编码 Agent 的终端。

v1 必须做到：

- 配对一次后，打开 App 直接回到上次的窗格。
- 查看并输入终端，支持 Codex、Claude Code、Opencode 等 TUI。
- 手机接管当前窗格的 PTY 尺寸，保证手机上可读。
- 浏览项目和其他窗格，必要时把某个窗格显示到桌面。
- 手机退到后台或断开时，桌面立即恢复原来的尺寸。
- 公网可用：不暴露端口，不依赖同一局域网。

v1 不做：

- 手机端独立 spawn 一个新的 PTY。
- 在手机上镜像桌面的分屏网格。
- 文件浏览、图片粘贴、端口转发。
- APNs 后台推送、App Store 上架。
- 多用户/团队共享。
- 通过远程接口执行任意命令或访问任意路径。

## 2. 已定决策

| 决策 | 选择 | 原因 |
| --- | --- | --- |
| 产品重心 | 终端优先 | 手机主要用来操作 TUI，不是只看状态 |
| 终端尺寸 | 手机接管 PTY | 手机宽度下可读、可输入 |
| 桌面表现 | 显示角标，继续可用 | 不阻断桌面工作 |
| 浏览同步 | 手机独立浏览 | 随便看看不打断桌面 |
| 后台行为 | 立即释放尺寸 | 桌面不长期停在手机尺寸 |
| 终端渲染 | 先用 NSH Terminal | 最快验证闭环，后续可替换 |
| 配对 | 粘贴链接 + 配对码 | NSH 0.5 没有现成相机组件 |
| 界面语言 | 中文 | 与桌面端一致 |
| 传输 | iroh | 加密 QUIC、NAT 穿透、relay 兜底 |

## 3. 信息架构

```text
启动
├─ 未配对                    → 配对页
├─ 已配对，有上次窗格        → 终端
└─ 已配对，没有上次窗格      → 窗格面板
```

页面只有四个：

1. 配对
2. 终端（首页）
3. 窗格面板（底部抽屉）
4. 设置

不做独立“收件箱”Tab。未读用窗格行上的圆点和应用内提醒表达。

## 4. 页面线框

### 4.1 配对

```text
┌───────────────────────────────┐
│  Kumu                     ⚙︎  │
│                               │
│  连接你的 Mac                  │
│  在 Mac 上打开 Kumu → 设置 →   │
│  远程，复制配对链接。          │
│                               │
│  ┌─────────────────────────┐  │
│  │ 粘贴配对链接或配对码     │  │
│  └─────────────────────────┘  │
│                               │
│  [          配  对         ]  │
│                               │
│  已配对                       │
│  MacBook Pro                  │
│  指纹 AB12…9F                 │
│  [重新连接]                   │
│                               │
│  首次连接需要在 Mac 上允许。   │
└───────────────────────────────┘
```

交互：

- 输入内容自动识别 `kumu://pair/...` 链接或 8 位配对码。
- 解析后显示 Mac 名称、endpoint 指纹、配对码，确认后才写入 Keychain。
- 失败文案区分：链接无效、配对码过期、Mac 未开启远程、网络不可达。
- 已配对但连接失败时保留上次的 Mac，不清空配对信息。

### 4.2 终端

```text
┌───────────────────────────────┐
│ ☰  kumu › Codex · 实现远程协议 │
│              ● 运行中      ⋯  │
├───────────────────────────────┤
│                               │
│  Codex                        │
│  > 正在读取项目文件…          │
│                               │
│                               │
│                               │
├───────────────────────────────┤
│ ● 已连接 · 48×40              │
├───────────────────────────────┤
│ Esc  Tab  ⌃  ⌥  ← ↑ ↓ →  粘贴 │
└───────────────────────────────┘
```

交互：

- 左上角 `☰` 打开窗格面板，不是返回连接页。
- 标题行显示“项目 › 窗格标题”；Agent 用图标区分。
- 状态点：运行中 / 等待确认 / 空闲 / 已退出 / 重连中。
- 底部是键盘工具栏；`⌃`、`⌥` 是粘滞键，点一次生效一次。
- `⋯` 菜单：在桌面显示、新建终端、新建 Agent、关闭窗格、字号、断开连接。
- 终端退出或窗格被关闭时，回到窗格面板并提示。

连接状态覆盖层：

```text
┌───────────────────────────────┐
│                               │
│        正在连接 Mac…           │
│                               │
│         [取消]                 │
│                               │
└───────────────────────────────┘

┌───────────────────────────────┐
│                               │
│      连接已断开                │
│      [重试]  [打开窗格]        │
│                               │
└───────────────────────────────┘
```

### 4.3 窗格面板

```text
┌───────────────────────────────┐
│  所有窗格                完成  │
│                               │
│  kumu                         │
│  ● Codex · 实现远程协议        │
│    等待确认 · 2 分钟前     ●   │
│  ○ Claude · 修复登录           │
│    运行中 · 刚刚               │
│  ⌁ shell                       │
│    空闲 · 15 分钟前            │
│                               │
│  herdr-mobile                 │
│  ● Opencode · 补测试           │
│    空闲 · 1 小时前             │
│                               │
│  [新建终端]      [新建 Agent]  │
└───────────────────────────────┘
```

交互：

- 按项目分组；worktree 行缩进，并带 branch 图标。
- 行内显示 Agent/终端图标、标题、状态、最后活动时间、未读点。
- 点行 → 进入终端并 attach。
- 底部“新建终端/新建 Agent”在当前项目、当前焦点窗格旁创建，具体分屏位置由桌面决定。
- 空状态：`这个项目还没有终端` + 新建按钮。

### 4.4 设置

```text
┌───────────────────────────────┐
│  设置                   完成   │
│                               │
│  已连接的 Mac                  │
│  MacBook Pro                  │
│  指纹 AB12…9F                 │
│  [断开]              [移除]    │
│                               │
│  终端                         │
│  字号        13      [−] [+]  │
│  主题        跟随系统          │
│                               │
│  安全                         │
│  [✓] 打开时使用 Face ID        │
│                               │
│  关于                         │
│  版本 0.1.0                   │
└───────────────────────────────┘
```

v1 只保留必要项；主题先跟随系统，不做多套配色。

## 5. 桌面端配套

### 5.1 设置 → 远程

```text
┌──────────────────────────────────────────┐
│ 远程控制                                  │
│                                          │
│ [✓] 允许 iPhone 连接                     │
│                                          │
│ 状态   运行中 · iroh 8f2a…               │
│ 配对   ABCD-EFGH（9:42 后过期）           │
│        [重新生成]  [显示二维码]           │
│                                          │
│ 设备   iPhone 15 Pro · 2 分钟前  [撤销]   │
│                                          │
│ 当前   iPhone 15 Pro 正在控制             │
│        kumu › Codex · 48×40      [断开]   │
└──────────────────────────────────────────┘
```

- 默认关闭；打开后才创建 iroh endpoint 和配对码。
- 关闭远程时断开所有设备，但保留已配对记录（下次打开仍可用）。
- 撤销设备立即断开该设备的连接；已撤销的 Token 不能重连。
- 设备列表只显示名称、最后在线时间，不显示 Token。

### 5.2 窗格角标

```text
┌───────────────────────────────────────────┐
│ Codex · 实现远程协议   📱 iPhone · 48×40  │
└───────────────────────────────────────────┘
```

- 角标表示当前窗格有远程控制租约。
- 点击角标：`断开远程控制` / `恢复桌面尺寸`。
- 桌面仍可输入；两个输入源不做互斥。
- 桌面窗口尺寸变化时只记录，不应用，等租约释放后恢复。
- M1 采用 last-wins：新连接接管旧租约，旧设备收到关闭；只读模式留到后续。

### 5.3 多设备和租约

- 每个窗格最多一个控制租约。
- 同一设备可以 attach 一个窗格；切换窗格时先释放旧租约。
- 新连接 attach 同一窗格时先释放旧 tap，再按新网格重排。
- 客户端每 10 秒发一次 `ping`；30 秒没有消息时服务端断开并释放租约。
- 断线或 App 退到后台时释放租约。

## 6. 协议 v1

### 6.1 传输

- iroh 1.4.x，ALPN `kumu/remote/1`。
- 一条 QUIC 双向流承载全部消息；控制通道 0，终端通道大于 0。
- 身份和配对状态放在 `~/.kumu/remote/`（0700；identity、devices 文件 0600）。
- 默认使用公共 relay 做开发；生产可配置自建 HTTPS relay。
- 不使用明文 TCP/WebSocket 监听；不为公网开放端口。

配对链接：

```text
kumu://pair/<base64url(JSON)>

{
  "version": 1,
  "address": { "id": "<endpoint public key>", "addrs": [...] },
  "discovery": true,
  "code": "ABCD-EFGH"
}
```

链接里最多保留 1 个 relay 和 2 个 IP，避免二维码过大。`code` 一次性、10 分钟有效、最多错 5 次。

### 6.2 帧格式

```text
u32 channel (little endian) | u8 kind | u32 length (little endian) | payload
```

- channel 0：UTF-8 JSON 控制消息，上限 256 KiB。
- channel > 0：终端数据，上限 2 MiB。
- 终端 kind：
  - `0 DATA`：PTY 输出 / 用户输入
  - `1 CREDIT`：客户端已消费的字节数
  - `2 CLOSE`：关闭通道，payload 可带 JSON 原因
  - `3 RESIZE`：`u16 cols | u16 rows`
  - `4 REPLAY`：服务端生成的 ANSI 首屏；客户端清空后写入
- 读取时先校验长度再分配；关闭的连接不能静默丢输入。
- 客户端消费完终端字节后回 CREDIT；服务端队列有界，溢出则关闭通道，客户端重新 attach。

### 6.3 控制方法

```text
hello              { protocol, token?, deviceName? }
pair               { code, name }
workspace.state
workspace.selectProject { project }
workspace.selectPane    { pane }
pane.attach        { pane, mode: "control", cols, rows }
pane.detach        { channel }
workspace.action   { type, ... }
events.subscribe
```

M1 只实现 `hello`、`pair`、`ping`、`workspace.state`、`pane.attach`、`pane.detach`、`workspace.action`；`workspace.selectProject`、`workspace.selectPane`、`events.subscribe` 留到 M4。客户端在 M1 通过 `workspace.action { type: "showOnDesktop" }` 切换项目和窗格。

`workspace.state` 返回：

```json
{
  "computer": { "name": "MacBook Pro", "id": "..." },
  "projects": [
    { "id": 1, "name": "kumu", "path": "~/github/kumu", "worktree": false }
  ],
  "panes": [
    {
      "id": 1,
      "project": 1,
      "kind": "agent",
      "agent": "codex",
      "title": "实现远程协议",
      "cwd": "~/github/kumu",
      "status": "waiting",
      "unread": true,
      "lastLine": "等待确认：是否允许运行测试？",
      "lastActivity": 1763000000,
      "cols": 48,
      "rows": 40
    }
  ],
  "selectedProject": 1,
  "focusedPane": 1,
  "recentPane": 1,
  "version": 12
}
```

`workspace.action` 只接受白名单：

```text
newTerminal  { project? }
newAgent     { project?, agent }
closePane    { pane }
showOnDesktop{ pane }
```

不提供任意命令、文件路径、粘贴桌面剪贴板等接口。

### 6.4 事件

服务端主动推送：

```text
workspace.changed
pane.status       { pane, status, unread }
pane.exited       { pane, project }
notification      { id, pane, project, title, body, createdAt }
lease.changed     { pane, holder }
pong
```

客户端收到 `workspace.changed` 后重新拉 `workspace.state`；终端的逐字节更新只走二进制通道。

M1 尚未推送这些事件：客户端轮询 `workspace.state`，服务端每秒最多刷新一次快照。事件推送随 `events.subscribe` 在 M4 接入。

### 6.5 认证

- `hello` 无 Token：返回 `needsPairing` 和 bridge 信息。
- `hello` 带有效 Token：返回设备信息和 bridge 信息。
- Token 32 字节随机值；Mac 只存 SHA-256 哈希，并绑定手机 iroh 公钥。
- 配对成功后 Tokens 写入 iOS Keychain；Mac 上撤销立即断开并拒绝重连。
- 配对码只显示在桌面设置里，不写日志。

### 6.6 Attach 生命周期

`pane.attach` 必须是一个原子操作，在终端 session 线程上完成：

1. 校验窗格和租约。
2. 记录桌面当前尺寸。
3. resize VT/PTY 到手机尺寸。
4. 注册订阅者并暂停转发。
5. 取当前屏幕快照，生成 ANSI 重放。
6. 返回 channel、实际 cols/rows、标题、状态。
7. 发送 `REPLAY`（ANSI），再发送暂停期间缓冲的实时字节。

客户端进入终端时：

- 创建空的 NSH Terminal。
- 写入重放。
- 之后按 CREDIT 消费实时字节。
- 手机旋转/键盘变化时发 `RESIZE`；PTY 跟随。
- 重连只重放屏幕，不重放未确认输入。

`pane.detach`、连接断开、App 后台、心跳超时：

- 释放租约。
- 恢复桌面记录的尺寸。
- 手机端保留本地画面，回前台重新 attach。

### 6.7 终端渲染数据流

远程不传 `Snapshot`，也不传画面。稳态传输的是 PTY 原始字节，iOS 端用自己的 VT 模拟器重新渲染。

```text
桌面本地：

PTY
 │ read_pty()
 ▼
session thread
 ├─ Vt::write(bytes) → Snapshot → TerminalView (GPUI)
 └─ remote tee        → frames  → iroh → iPhone
                                          │
                                          ▼
                                   NSH Terminal
                                   VTE → cell grid → CoreText/UIKit
```

一次 attach 的时序：

```text
iPhone                    Mac remote                 session thread
  │ pane.attach {cols,rows} │                           │
  ├────────────────────────>│ 设置租约                   │
  │                         ├──────────────────────────>│ 订阅输出
  │                         │                           │ resize PTY/VT
  │                         │                           │ 取 Snapshot
  │                         │<──────────────────────────┤ 生成 ANSI replay
  │<── attach reply ────────┤                           │
  │<── REPLAY (ANSI) ───────┤                           │
  │                         │<── 实时字节 ──────────────┤
  │<── DATA ────────────────┤                           │
  │ 写入 NSH Terminal       │                           │
  │ 回 CREDIT ─────────────>│                           │
```

顺序必须是“先订阅，再 resize，再快照，再重放，最后放行缓冲字节”，否则会漏数据或让重放和实时输出交错。

重放内容：

- 清屏、光标归位。
- alternate screen、DECCKM、bracketed paste、mouse tracking 等模式。
- 每一行的 SGR 颜色/样式和文本。
- 光标位置和可见性。
- **不包含**桌面 scrollback；v1 手机历史从 attach 开始。

三种可选方案：

| 方案 | 传什么 | iOS 端 | 取舍 |
| --- | --- | --- | --- |
| 原始字节（v1） | PTY bytes + `REPLAY` | NSH Terminal 自己解析 | 带宽低、选择/滚动/输入由终端组件处理；需要模式恢复 |
| Cell 快照 | 每帧 rows/cells/styles | 自己画网格 | 画面完全一致、无 reflow；带宽高，选择/IME/滚动都要自己做 |
| 服务端 ANSI diff | 桌面编码脏行 | NSH Terminal 解析 | 比 cell 快照省带宽；桌面多做 diff，输入模式问题仍在 |

输入路径：

```text
NSH Terminal on_data(bytes)
  → DATA frame
  → session thread
  → PTY
```

手机端已经按自己的终端模式编码了按键，所以桌面不再走 `Vt::encode_input`，直接写 PTY 原始字节。

两个必须处理的边界：

- **重复查询应答**：桌面 `Vt` 和 NSH Terminal 都可能响应 DSR/DA，PTY 会收到两份。先观察；TUI 乱码时提前换 native Ghostty 或过滤自动应答。
- **模式缺失**：手机只看到 attach 之后的模式。重放必须带 alternate screen、方向键模式、bracketed paste 等，否则方向键和粘贴会编码错误。

### 6.8 错误

```json
{ "error": { "code": -32000, "message": "...", "data": { "kind": "..." } } }
```

`kind` 至少包括：

```text
unauthorized        重新配对
pairing_failed      配对码错误/过期
protocol_mismatch   升级 App
pane_gone           窗格已关闭
lease_taken         另一台设备正在控制（保留给后续只读模式，M1 使用 last-wins）
overflow            客户端消费太慢，需要重新 attach
offline             Mac 上的 Kumu 没运行
```

### 6.9 时序示例

以下省略 JSON-RPC 的 `jsonrpc` 字段，只保留关键内容。控制消息走 channel 0，终端帧走 attach 返回的 channel。

#### 首次配对

```text
iPhone → Mac   {"id":1,"method":"hello","params":{"protocol":1,"deviceName":"iPhone"}}
Mac → iPhone   {"id":1,"result":{"needsPairing":true,"bridge":{"name":"MacBook Pro","endpointId":"...","protocol":1}}}
iPhone → Mac   {"id":2,"method":"pair","params":{"code":"ABCD-EFGH","name":"iPhone"}}
Mac → iPhone   {"id":2,"result":{"token":"...","device":{"id":"d1","name":"iPhone"},"bridge":{"name":"MacBook Pro","endpointId":"...","protocol":1}}}
```

- 配对成功后当前连接直接进入已认证状态。
- iPhone 把 Token 存 Keychain，不打印。
- 配对码立即作废；桌面生成下一个码。

#### 重连

```text
iPhone → Mac   {"id":1,"method":"hello","params":{"protocol":1,"token":"..."}}
Mac → iPhone   {"id":1,"result":{"device":{"id":"d1","name":"iPhone"},"bridge":{...},"protocol":1}}
iPhone → Mac   {"id":2,"method":"workspace.state"}
Mac → iPhone   {"id":2,"result":{ ...projects/panes/selected/focused... }}
iPhone → Mac   {"id":3,"method":"pane.attach","params":{"pane":1,"mode":"control","cols":48,"rows":40}}
```

- Token 无效或已撤销：`error.data.kind = "unauthorized"`，iPhone 回到配对页。
- 协议不匹配：`protocol_mismatch`，提示升级。

#### Attach

```text
iPhone → Mac   {"id":3,"method":"pane.attach","params":{"pane":1,"mode":"control","cols":48,"rows":40}}

Mac 内部：设置租约 → 订阅输出 → resize PTY/VT → 取快照 → 生成 ANSI

Mac → iPhone   {"id":3,"result":{
                 "channel":7,
                 "cols":48,
                 "rows":40,
                 "credit":262144,
                 "title":"Codex · 实现远程协议",
                 "status":"working",
                 "lease":"control"
               }}
Mac → iPhone   channel=7 kind=4 (REPLAY, ANSI)
Mac → iPhone   channel=7 kind=0 (DATA, 缓冲期间的实时字节)
```

- iPhone 先按 `result` 创建终端，写 `REPLAY`，再处理后续 `DATA`。
- `REPLAY` 也计入 credit；iPhone 消费完回 CREDIT。
- 桌面窗口在租约期间不再 resize 这个窗格，只记录自己的尺寸。

#### 输入、输出与 credit

```text
iPhone → Mac   channel=7 kind=0 (DATA, "ls\r")
Mac → PTY      写入原始字节
PTY → Mac      输出字节
Mac → iPhone   channel=7 kind=0 (DATA, 输出)
iPhone         写入 NSH Terminal
iPhone → Mac   channel=7 kind=1 (CREDIT, u32 已消费字节)
```

- 输入是 best-effort，不排队重放。
- 输出队列有界；超出 credit 时服务端停下，不静默丢字节。
- 长时间跟不上：服务端发 `CLOSE {reason:"overflow"}`，客户端重新 attach。

#### 旋转 / 键盘变化

```text
iPhone         NSH Terminal on_resize(60, 30)
iPhone → Mac   channel=7 kind=3 (RESIZE, cols=60, rows=30)
Mac            校验租约 → resize PTY/VT
Mac → iPhone   channel=7 kind=0 (程序重绘输出)
```

- 桌面窗口尺寸变化不会发 RESIZE；只更新桌面记录尺寸。
- 手机连续旋转时以最后一次为准，不需要 ack。

#### 退到后台

```text
iPhone → Mac   {"id":4,"method":"pane.detach","params":{"channel":7}}
Mac → iPhone   {"id":4,"result":{}}
Mac            释放租约 → 恢复桌面尺寸
Mac → iPhone   channel=7 kind=2 (CLOSE, {"reason":"detached"})
```

连接直接断开而不发 `detach` 时，服务端也必须释放租约。回前台重新 `hello` + `pane.attach`。

#### 断线重连

```text
连接断开
iPhone         不重放未确认输入
iPhone → Mac   hello(token)
iPhone → Mac   workspace.state
iPhone → Mac   pane.attach {pane, cols, rows}
Mac → iPhone   新的 channel + REPLAY
```

- 如果断线前输入已经写入 PTY，可能已经执行；客户端不自动重试。
- 重连后窗口标题、状态、未读数以 `workspace.state` 为准。

#### 错误与冲突

```text
窗格已关闭：
  pane.attach → error.data.kind = "pane_gone"
  iPhone 回窗格面板并提示

另一台设备正在控制：
  M1 新连接释放旧 tap 后直接接管（last-wins）
  旧设备收到 channel CLOSE，回窗格面板

Mac 上的 Kumu 没运行：
  连接失败 → iPhone 显示 "Mac 上的 Kumu 没有运行"

客户端消费太慢：
  channel CLOSE {reason:"overflow"}
  iPhone 重新 attach，拿到新的 REPLAY
```

#### 桌面动作

```text
iPhone → Mac   {"id":10,"method":"workspace.action",
                "params":{"type":"newAgent","project":1,"agent":"codex"}}
Mac            在当前焦点窗格旁创建并启动 Agent
Mac → iPhone   {"id":10,"result":{"ok":true}}
Mac → iPhone   {"method":"workspace.changed","params":{"version":13}}
iPhone → Mac   workspace.state
```

- 新建后不自动 attach；用户在窗格面板里选择。
- `showOnDesktop` 只选择项目和焦点窗格，并激活桌面窗口，不改变手机连接。

## 7. 状态模型

每个窗格：

| 状态 | 判定 |
| --- | --- |
| `working` | 最近 2 秒有 PTY 输出，或 Agent hook 报告开始工作 |
| `waiting` | 有未处理的确认/提问通知 |
| `idle` | 没有输出、没有待处理通知 |
| `exited` | 进程退出 |
| `unknown` | 刚连接，还没有足够信息 |

未读和状态分开：`waiting` 不一定未读，`unread` 表示手机离开该窗格后发生过需要关注的事件。

v1 的状态是启发式的，不承诺准确区分 Agent 的“思考/执行/等待”。等各 Agent 的 hook 补齐后再细化。

## 8. 安全

终端输入等于在 Mac 上执行命令，安全不能后补。

- 远程默认关闭，开启后才启动服务。
- 配对码一次性、限时、限次数。
- Token 绑定 iroh 公钥，Mac 只存哈希；Keychain/文件权限最小化。
- 设备列表可撤销；撤销立即生效。
- 不做公网端口映射；公网走 iroh relay。
- 动作 API 白名单；不接受任意路径和命令。
- 建议支持 Face ID 解锁 App（可选）。
- 日志不打印配对码、Token、终端内容。

## 9. NSH 实现约束与风险

### 9.1 有利点

- NSH Terminal 按视图自动 resize，正好对应“手机接管尺寸”。
- `on_data` 提供用户输入和终端查询应答，`on_resize` 提供网格变化。
- NSH 支持 `InputAccessoryView`，键盘工具栏可以原生呈现。
- 现有 `kumu-mobile` 已经有连接页 + 全屏终端的可运行切片。

### 9.2 风险

| 风险 | 影响 | v1 处理 |
| --- | --- | --- |
| 两个终端模拟器都回答查询 | PTY 收到重复 DSR/DA 应答 | 先接受；TUI 乱码时提前换 native Ghostty 或过滤 |
| NSH Terminal reflow | 复杂 TUI 的行宽/光标可能异常 | 实测 Codex/Claude/Opencode；记录失败样例 |
| 无相机组件 | 不能扫码配对 | v1 粘贴链接 + 配对码；后续 native 组件 |
| 滚动历史不同步 | 手机上只能看 attach 之后的输出 | 明确为本地滚动；不同步桌面 scrollback |
| 后台挂起 | WebSocket/QUIC 会被切断 | 后台立即 detach；回前台重新 attach |
| 无 APNs | 后台不能实时提醒 | v1 只做前台提醒；后续评估 APNs |

### 9.3 终端渲染的退路

如果 NSH Terminal 在真实 Agent TUI 上不稳定：

1. 先保留 NSH 页面，把终端换成 native Ghostty 组件。
2. 如果 native 组件接入成本过高，再评估把应用外壳改回 Swift/UIKit + Ghostty（herdr-mobile 走过的路）。

## 10. 桌面实现边界（设计层）

- 新增 `src/remote/`：iroh 服务、配对、设备、协议。
- 服务运行在独立 tokio 线程；不阻塞 GPUI 事件循环。
- `RemoteHub` 保存项目/窗格状态和最近快照；UI 更新它，服务端读取并广播。
- UI 通过命令通道处理服务端动作（选择项目、关闭窗格等）。
- `Term` 增加输出订阅，attach 时在 session 线程完成 resize + 快照 + 重放 + 订阅。
- 尺寸租约放在 Workspace/KumuApp：远程租约存在时忽略桌面 `terminal_resize`，只更新桌面记录尺寸。
- 桌面设置页和窗格角标只读远程状态，不直接操作网络。

## 11. 里程碑

1. **M0 设计**：本文档、线框、协议评审。
2. **M1 桌面骨架**：iroh、配对、state、attach/input、租约；用 Rust 测试客户端验证。
3. **M2 NSH 客户端**：配对页、终端页、窗格面板、快捷键栏。
4. **M3 桌面 UI**：远程设置页、设备撤销、窗格角标、断开。
5. **M4 稳定性**：重连、后台 detach、状态启发式、错误文案、TUI 兼容性测试。
6. **M5 后续**：二维码扫描、Face ID、APNs、iPad 双栏、native 终端评估。

## 12. M1 实现状态

已完成：

- 桌面端 `src/remote/`：iroh endpoint、配对码、设备 Token、`workspace.state`、`pane.attach` / `input` / `resize` / `detach`。
- 终端输出在 session 线程直接 tee 给远程订阅者；attach 时先 resize、再取快照、生成 ANSI 重放、再放行实时字节。
- 尺寸租约：手机 attach 后桌面记录原尺寸并暂停覆盖；detach 或新连接接管时恢复。
- NSH 客户端：粘贴配对链接、设备身份持久化、Token 存储、首次布局后按真实网格 attach、NSH Terminal 渲染和输入。
- 终端页：`☰` 窗格抽屉（按项目分组）、`⋯` 操作菜单、项目和状态、点击切换、两行按键区、粘滞 Ctrl/Alt。
- 操作菜单：在桌面显示、新建终端、新建 Agent（已安装列表）、字号 +/−、关闭窗格（系统确认）、断开连接；新建/关闭后刷新窗格列表。
- 集成测试：配对 → state → attach → replay/live → input → detach。
- 远程服务随应用启动，无需环境变量；侧栏底部的手机图标打开配对卡片，显示当前配对码和配对链接，可分别复制。
- 状态启发式：窗格最近 2 秒有输出记为 `working`（`lastActivity` 为最近输出的时间戳），有未读通知记为 `waiting`。

M1 已知限制：

- 窗格抽屉按项目分组，但还没有折叠/展开。
- Action 失败只在日志记录，界面没有提示。
- Ctrl/Alt 只转换单字符输入，多字符粘贴和 IME 不转换。
- Token 存在 NSH `Storage`，尚未迁移 Keychain。
- 心跳间隔 10 秒、租约超时 30 秒；last-wins 保证重连不会卡住。
- 桌面设置页、设备撤销和二维码尚未接入。
- 手机不发送 CREDIT 之外的背压控制；溢出会断开通道。
- 终端查询应答重复、scrollback 不同步仍按 6.7 的风险处理。

## 13. 待定

- 自建 relay 的部署位置和域名（v1 可先用公共 relay）。
- 是否需要“只读/跟随尺寸”模式给第二台设备。
- 窗格标题的截断规则和多语言标题。
- iPad 是否在 v2 做侧栏 + 终端双栏。
- Agent 工作状态的准确来源（hook 事件 vs 输出活动）。
