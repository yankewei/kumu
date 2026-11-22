# AGENTS.md
A macOS workspace that runs local coding agents in their own terminal interfaces, one project list, project tabs and a splitable grid of PTYs per tab. Each tab is a whole layout; switching tabs keeps the hidden terminals running, and the tab order and selected tab are saved with the workspace.

# 原则
1. 用户体验优先
2. 有需求先进行设计再进行开发

## Constraints
1. Build all business UI (sidebar, tabs, splitable workspace shell, pane headers, `⌘F` find bar, dialogs, settings, toasts, tooltips, plugin panels, diff sheet) with Pure Rust `nsh` (`nsh-core` + `nsh-base` + `src/nsh_ui/` wrappers) hosted on `nsh-platform-gpui`; reserve bare `gpui` (`gpui-fast`) exclusively for the macOS window/titlebar shell (`src/ui/titlebar.rs`), native `SystemNotification`, and the GPU-accelerated Ghostty VT `TerminalView` embedded as an NSH `NativeView` (do not re-introduce `gpui-kit`)
2. Keep core domain logic (`src/app/core.rs`, `src/app/layout.rs`, `src/term/`, `src/agent/`, `src/db.rs`, `src/git.rs`, `src/control.rs`) free of UI framework dependencies
3. 在写测试的时候只需要写有必要的测试，没有必要覆盖非常边缘的可能性

## Type scale
Stay on the two-step rem scale; no custom `text_size(px(...))` or `text_base`+ in app code:
- `text_sm` (14px) — body text: message content in the conversation, card/section titles (pair with `font_medium`/`font_bold`), main empty states ("还没有项目"), window title bar
- `text_xs` (12px) — everything secondary/meta: sidebar project rows, composer chip labels (model name), the model picker's list rows (name + provider subtitle, name keeps `font_medium`), timestamps, inline labels and captions, empty list placeholders
- Hierarchy within the same size comes from `font_medium`/`font_bold` and muted foreground color, not from mixing extra sizes
- Input components set their own font (theme mono size); don't override their text size

## Product reference
- Use [zed](https://github.com/zed-industries/zed) and [herdr-app](https://github.com/herdrdev/herdr-app) source code on github as a reference when you need to design a UI or build a feature

## Doc reference
- NSH 框架文档：https://nsh.dev/docs/（AI 查阅请优先使用纯文本索引 https://nsh.dev/llms.txt 与任意页面 `.md` 原文，如 `https://nsh.dev/docs/rust.md`）
- NSH 官方开发与设计技能已内置在 `.agents/skills/nsh/SKILL.md` 与 `.agents/skills/nsh-design-guides/SKILL.md`（与 `Cargo.toml` 锁定的 NSH commit `ebfdfad` 同步）；新增或改造 NSH 界面前请优先遵循其响应式粒度、交互状态与无头测试规范

## macOS releases
- 生成可分发的 macOS DMG 或发布新版本时，用 `packaging/release.sh`（`--publish` 才打标签上传）；步骤和前置条件见 `.agents/skills/release-macos/SKILL.md`。默认用 Developer ID Application 身份本地签名、只公证 DMG 一次（`KUMU_SIGNING_P12` 或钥匙串里的身份）；没有身份时才回退到 Xcode 云端签名，慢约一分钟。
- `make-app.sh`/`make-dmg.sh` 只做本地测试包，不要在云端签名后重跑它们覆盖正式产物。
