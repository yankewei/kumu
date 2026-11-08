# AGENTS.md
A client that drives multiple local coding agents, each over its own protocol, behind one agent layer.

## Tech stack
- gpui-kit, the entire UI: GPUI rendering engine (wgpu drawing, text layout), declarative element tree, component library (Button/Input/Switch/TitleBar/Root), theme system (ThemeRegistry + semantic tokens), multi-window management. Hover labels on app elements go through an overlay of the app's own (src/ui/tooltip.rs, mounted by src/ui/layers.rs::`window_layers`) because the one `Root` mounts is crate-private — the tooltip surface and the overlay are both public API, the trait that asks the overlay for a tooltip is not

## Constraints
1. Build the UI with gpui-kit components first
2. 在写测试的时候只需要写有必要的测试，没有必要覆盖非常边缘的可能性

## Type scale
Stay on the two-step rem scale; no custom `text_size(px(...))` or `text_base`+ in app code:
- `text_sm` (14px) — body text: message content in the conversation, card/section titles (pair with `font_medium`/`font_bold`), main empty states ("还没有会话"), window title bar
- `text_xs` (12px) — everything secondary/meta: sidebar project & session rows, composer chip labels (model name), the model picker's list rows (name + provider subtitle, name keeps `font_medium`), timestamps, inline labels and captions, empty list placeholders
- Hierarchy within the same size comes from `font_medium`/`font_bold` and muted foreground color, not from mixing extra sizes
- Input components set their own font (theme mono size); don't override their text size

## Product reference
- Use [zed](https://github.com/zed-industries/zed) and [herdr-app](https://github.com/herdrdev/herdr-app) source code on github as a reference when you need to design a UI or build a feature

## macOS releases
- 生成可分发的 macOS DMG 或发布新版本时，先读取并执行 [packaging/RELEASING.md](packaging/RELEASING.md)。默认使用已验证的 Xcode 云端 Developer ID 签名流程；不要把缺少本机 Developer ID 私钥直接视为阻断。
