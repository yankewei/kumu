# kumu — design brief

macOS 桌面壳：在一个窗口里驱动本机的多个编码 agent，按 projects → sessions → conversation 组织。界面语言中文，代码与标识符英文。

## Register

**product**（仪表，不是品牌页）。界面本身就是工具：日常要打开、要扫读、要在心里不假思索地操作。

唯一的品牌露出是两处窗口标题栏的 `kumu` 字标（`src/ui/home/mod.rs:1014`、`src/ui/settings.rs` 的 `TitleBar`），`text_sm` + `font_bold`，不加 logo、不加副标题。不要往品牌方向扩张。

## Users and context

- **用户**：本机同时开着多个编码 agent（opencode / Oh My Pi / Codex / Claude Code / Pi）的开发者，自己写代码，也自己排 agent 的故障。
- **状态**：多个会话并行跑。一个回合在流式输出，另一个会话可能正卡在权限审批上等他一句话；注意力被切成几份，视线在侧边栏与对话之间来回。
- **由此产生的硬要求**：决策入口位置固定（不能随滚动跑掉）；"谁在动、谁在等我"一眼可读；工具调用不许把屏铺满。

## Product purpose

项目 → 会话 → 对话，全部在一个窗口内完成。agent 之间的协议差异止步于 backend 边界，UI 只认一种形状（AGENTS.md 约束 2）。

## Voice

- UI 文案中文，代码/注释/标识符英文（刻意分离）。
- 极简祈使：单动词、纯文字。实际措辞就是 `删除`、`重命名`、`刷新`、`取消`、`返回` —— 不是 `删除会话`（`src/ui/home/sidebar.rs:144`）。不要句末感叹号，不要营销式补语。
- 状态/兜底文案要自解释"会发生什么"，并点明**是谁的**配置：`跟随 Claude Code 配置`（`src/ui/widgets.rs` 的 `inherited_label`）。读起来像配置文件名或内部术语的写法一律不行。
- 空状态要给出下一步动作，而不只是陈述为空：`还没有会话` 配 `新建会话`（`src/ui/home/mod.rs:1003-1009`）。
- 进行中文案点出实际在做的事：`思考中…`、`检测模型…`、`模型未知`。

## Anti-references

- 通用 AI/SaaS 反射：居中 hero、卡片网格、磨砂药丸按钮、蓝紫渐变 CTA。
- 装饰性图标（菜单项、列表项尤其）、为营造"呼吸感"而存在的留白、无依据的固定宽高（`w(px(280.))` 那类）。
- "开发工具 = 黑暗终端等宽"的类别反射。主题是既有资产（`themes/*.json`），在此之上不许另造一套视觉语言。
- 把 agent/协议差异泄漏进 UI，让界面按"现在接的是谁"分支。
- 为了让界面好看而引入 Node 运行时或重型前端依赖：这是纯 Rust 原生应用，代价不接受。

**参考实现**（照 AGENTS.md 的产品参考）：Zed 与 waku。密度、字号缩放方式、设置窗的形态都对齐它们，而不是自创。

## Design principles

1. **颜色只有一种来源。** 一律 `cx.theme()` 按语义角色取 token（`sidebar_*` 归侧边栏），app 代码里不出现裸 hex/hsla（`src/main.rs:5-9`）。
2. **布局用 rem，px 只留给物理边界。** `w_64`/`p_4`/`gap_3`/`text_sm`；px 仅用于 1px 发丝线（`h_px`/`w_px`）与窗口尺寸。
3. **两级字阶，不许自造。** `text_sm`(14px) 承载正文与卡片/区块标题（配 `font_medium`/`font_bold`）；`text_xs`(12px) 承载一切次级信息（侧边栏行、chip、时间戳、说明文字）。层级靠字重与 muted 前景色区分，不靠加字号；输入组件自带的字体字号不去覆盖。
4. **工具调用不成屏。** 连续的 tool-call 折叠成一行摘要（`N 个工具调用 · M 个失败`），展开才见细节；单条调用不折（没东西可摘要）。见 `render_tool_run`。
5. **决策入口不移位。** 权限审批卡固定在 composer 上方，不内联到会滚动的对话流里（`src/ui/home/permission.rs:1-6`）。宁可牺牲"挂在那次具体调用上"的关联性。
6. **不能生效的控件整块隐藏**，而不是置灰、不是显示兜底文案、不是"点了没反应"。判定事实要有单一来源（`drives_sessions` 之于 `session_axes_editable`）。
7. **尺寸由内容决定。** 容器/行按内容自适应并配 min/max 夹取；不为不存在的内容预留空间。
8. **进行中必须立刻可见。** 从触发到首个事件之间不留静止窗口：回合一开始就有 spinner + 计时（`思考中…` / `12s`），不能只有 `…`。
9. **用户的显式选择压过自动生成的值**，且持久生效、不被后续自动数据静默回滚（会话名 `title_manual`）。
10. **每个正交的轴各自拥有独立控件。** 不把一个轴塞进另一个轴的 chip 里借名。

## Accessibility expectations

**标准已定：硬门槛。** 无障碍不达标按缺陷处理，不是"顺手就做"。判定线：

- 每个流程都能**只用键盘走完**（发送、切换会话、展开/折叠、应答权限、设置项、窗口关闭）。
- 每个控件有**可被朗读的名字与角色**，VoiceOver 能读出状态（选中/展开/进行中）。
- 状态**不靠颜色单独承载**：颜色必须配图标、文字或形状。
- 焦点可见、顺序符合阅读顺序；不用 `tabindex` 正数去改顺序。

**核实过的现状：**

- 审批卡已达标：允许/拒绝各有文字标签（`允许一次` / `取消` 等），颜色只是加强。
- **待修缺陷**：侧边栏会话行的 `+` 只在 hover 时出现（`opacity(0)` → `group_hover` 到 1，`src/ui/home/sidebar.rs:69-85`）。hover-only 的功能入口违反上面的判定线——键盘用户与触控路径都到不了它。修法方向是让入口在有焦点/选中时同样可见，而不是取消这个入口。
- 焦点环、键盘遍历由 gpui-kit 提供，app 代码中未见 `outline: none` 之类的抹除（**未逐窗口实跑验证**，当前只做到源码层面确认）。

后续任何触及控件、表单、浮层或动效的改动，都按上述四条逐项验证，并在汇报里说明验证覆盖到哪一步。

## Visual foundation

- **主题**：`themes/light.json`（`Kumu Light`，启动时应用，含 system font / radius 6 / radius.lg 8）与 `themes/dark.json`（`Zed Dark`，radius 与密度沿用 Zed 的暗色取值）。经 `src/theme.rs` 载入 ThemeRegistry，无第三方主题机制。
- **字号缩放**：UI 字号是设置项（12–20px，9 档，默认 16），做法是改窗口 rem size，文字/图标/间距/行高**整体等比**缩放（对齐 Zed 的 `ui_font_size`），不做只放大文字的方案。
- **字体**：默认系统字体（`.SystemUIFont`），可由用户按已安装族切换。
- **强调色**：亮色 `#94a4f5`（periwinkle），暗色 `#62adef`（蓝）。两者都是完整语义集，不新增色相。
- **卡片/浮层**：设置页里卡片在暗色模式用 muted 面，亮色模式用白面（`settings.rs` 的 `card_bg`），浮层走 popover token。
- **Markdown 回复**：`TextView::markdown`，`text_sm` + `line_height(relative(1.75))`，段落间距 1.25rem（`install_text_view_style`）。
- **资源**：`assets/` 经 rust-embed 内嵌，叠在 gpui-kit 图标库之上。agent logo 分两类——svg 单色标记（Pi、Codex）按主题 tint，png 保留品牌原色；缺省回退 `IconName::Bot`。

## Component rules

- **chip**（`widgets.rs::pill`）：`px_2 py_0p5` + theme radius + `text_xs`。accent 态 = `primary.opacity(0.14)` 底 + `link` 字；neutral 态 = `muted` 底 + `muted_fg` 字。chip 只显示取值本身，不加类别前缀；可以带一个与语义相配的前置图标（model chip 带 agent logo、推理深度带 brain）。
- **composer 工具栏顺序**：`model · 推理深度 · 访问模式 · 工作模式 · 沙箱` —— 先放属于模型的轴，再放会话级的轴，会话级内部先"能自主到什么程度"再"此刻在做什么"。之后 flex 撑开，发送/暂停贴最右。
- **发送位复用**：回合进行中，发送按钮变成暂停按钮（同一个位置，同一个 primary 样式），点它即取消这一轮。
- **按钮语义**：primary 只留给"提交决策"（发送、允许一次）；拒绝用 danger；取消、应用内导航用 ghost/default。
- **列表行**（`widgets.rs::select_row`）：`text_xs` + `font_medium` + truncate 的名称，右侧可选 muted 副标题（无内容就不留位），选中态 accent 底。列表不展示来源提供的冗长说明。
- **对话列**：宽 `24rem–44rem` 夹取并居中（`CONTENT_MAX_REM` / `CONTENT_MIN_REM`）。用户消息右对齐、最宽 78%、`primary.opacity(0.1)` 底；系统消息是 muted `text_xs` 单行；思考/tool 折叠体限高自带滚动（思考与工具 16rem，权限详情 10rem）。
- **侧边栏行**：`text_xs`，选中态 `sidebar_accent`，等待审批用 6px 的 `warning` 圆点标记（不改变行高）。
- **两个窗口**：Home 1360×900（最小 960×640）；Settings 680×640（最小同尺寸，左栏 14rem 导航 + 搜索，右栏 `max_w(40rem)` 居中卡片）。

## Composition defaults

- **home 主工作区 = Operate**：左侧项目/会话树 + 右侧"对话（上）+ 决策卡（中）+ 输入（下）"的单列。所有动作直接操纵，没有向导、没有多步流程。
- **home 的次模式 = Monitor**：并行回合与待审批状态要在侧边栏一眼看见，而不是靠逐个点进去。
- **settings = Configure**：左导航（返回 / 搜索 / 图标项）+ 右栏分组卡片，行 = 标题 + 说明 + 右侧控件。不出现 hero、不出现推销式文案。
- **本项目不使用的构图**：居中 hero、营销分节、卡片网格、Decide 型单页（kumu 没有对外落地页）。

### Coverage gaps（目前不存在的界面）

- **没有"添加项目"**：`projects` 表只在列表为空时于启动时自动播一条（`src/ui/home/mod.rs:257-278`，`insert_project("kumu", cwd)`），用户无法选择目录、命名项目或加第二个项目。侧边栏那个 `+` 加的是会话。真要补，需要：侧边栏入口、目录选择（当前无原生文件夹选择依赖）、name 派生、与 `Store::projects()` 去重。
- **没有搜索/过滤**：设置页有搜索，主窗口的会话树没有。
- **没有项目级操作**：项目行只有折叠/展开与新建会话，无重命名/删除/右键菜单。

## How future design work should behave

- 先读本文件与 `.commandcode/taste/taste.md`，再动手；两者都指向"少装饰、位置稳定、状态自解释"。
- 涉及某条能力是否支持时，回到源码与 vendored schema 核实，不要从注释或推断下结论。
- 改完以 `cargo fmt` / `check` / `clippy` / `test` 全绿 + 一次启动烟测收尾，不留残留进程。

## 已知待修（按无障碍硬门槛排出的第一项）

侧边栏会话 `+` 的 hover-only 可见性（`src/ui/home/sidebar.rs:69-85`）。这是本文件定下硬门槛后暴露出的第一个真实缺陷，尚未修——`/design setup` 只建立设计记忆，不动代码。要修请单开一次 `/design a11y`。
