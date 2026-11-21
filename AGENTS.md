# AGENTS.md
A macOS workspace that runs local coding agents in their own terminal interfaces, one project list, project tabs and a splitable grid of PTYs per tab. Each tab is a whole layout; switching tabs keeps the hidden terminals running, and the tab order and selected tab are saved with the workspace.

# 原则
1. 用户体验优先
2. 有需求先进行设计再进行开发

## Constraints
1. Build business UI (windows, panels, sidebar, tabs, dialogs) with Pure Rust `nsh` (`nsh-core` + `nsh-base` + `src/nsh_ui/components.rs` wrappers) hosted on `nsh-platform-gpui`; reserve `gpui-kit` / `gpui-fast` for the macOS window shell (TitleBar, Notifications), splitable terminal grid (`h_resizable`/`v_resizable`), and the GPU-accelerated Ghostty VT `TerminalView`
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

## macOS releases
- 生成可分发的 macOS DMG 或发布新版本时，用 `packaging/release.sh`（`--publish` 才打标签上传）；步骤和前置条件见 `.agents/skills/release-macos/SKILL.md`。默认用 Developer ID Application 身份本地签名、只公证 DMG 一次（`KUMU_SIGNING_P12` 或钥匙串里的身份）；没有身份时才回退到 Xcode 云端签名，慢约一分钟。
- `make-app.sh`/`make-dmg.sh` 只做本地测试包，不要在云端签名后重跑它们覆盖正式产物。
