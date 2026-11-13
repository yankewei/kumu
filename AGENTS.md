# AGENTS.md
A macOS workspace that runs local coding agents in their own terminal interfaces, one project list and a splitable grid of PTYs per project.

## Tech stack
- gpui-kit, the entire UI: GPUI rendering engine (wgpu drawing, text layout), declarative element tree, component library (Button/Input/Switch/TitleBar/Root), theme system (ThemeRegistry + semantic tokens), multi-window management. Hover labels on app elements go through an overlay of the app's own (src/ui/tooltip.rs, rendered by each window) because the one `Root` mounts is crate-private — the tooltip surface and the overlay are both public API, the trait that asks the overlay for a tooltip is not. Dialogs, sheets and notifications are mounted by the Base `Root` from gpui-kit 0.7 on, so windows no longer add those layers themselves

## Constraints
1. Build the UI with gpui-kit components first
2. 在写测试的时候只需要写有必要的测试，没有必要覆盖非常边缘的可能性

## Type scale
Stay on the two-step rem scale; no custom `text_size(px(...))` or `text_base`+ in app code:
- `text_sm` (14px) — body text: message content in the conversation, card/section titles (pair with `font_medium`/`font_bold`), main empty states ("还没有项目"), window title bar
- `text_xs` (12px) — everything secondary/meta: sidebar project rows, composer chip labels (model name), the model picker's list rows (name + provider subtitle, name keeps `font_medium`), timestamps, inline labels and captions, empty list placeholders
- Hierarchy within the same size comes from `font_medium`/`font_bold` and muted foreground color, not from mixing extra sizes
- Input components set their own font (theme mono size); don't override their text size

## Product reference
- Use [zed](https://github.com/zed-industries/zed) and [herdr-app](https://github.com/herdrdev/herdr-app) source code on github as a reference when you need to design a UI or build a feature

## macOS releases
- 生成可分发的 macOS DMG 或发布新版本时，用 `packaging/release.sh`（`--publish` 才打标签上传）；步骤和前置条件见 `.agents/skills/release-macos/SKILL.md`，默认走已验证的 Xcode 云端 Developer ID 签名，不要把缺少本机 Developer ID 私钥视为阻断。
- `make-app.sh`/`make-dmg.sh` 只做本地测试包，不要在云端签名后重跑它们覆盖正式产物。
