# AGENTS.md
A client that communicates with multiple local agents over the ACP protocol.

## Tech stack
- gpui-kit, the entire UI: GPUI rendering engine (wgpu drawing, text layout), declarative element tree, component library (Button/Input/Switch/TitleBar/Root), theme system (ThemeRegistry + semantic tokens), multi-window management
- agent-client-protocol, the communication protocol with local agents: stdio subprocess handshake (initialize/session/new/prompt), permission requests, config option parsing, all in src/acp.rs
- rusqlite, persistence layer src/db.rs: projects/sessions/messages tables, bundles SQLite at compile time, no system library needed
- tokio, the dedicated async runtime thread for acp.rs: drives async stdio read/write and protocol request/notification streams; UI→ACP uses its unbounded channel
- serde + serde_json, two places: parsing theme JSON into ThemeRegistry; acp.rs reads opencode's auth.json for the API key

## Constraints
1. Build the UI with gpui-kit components first
2. Communication with local agents must go through the ACP protocol (Agent Client Protocol, stdio subprocess)

## Type scale
Stay on the two-step rem scale; no custom `text_size(px(...))` or `text_base`+ in app code:
- `text_sm` (14px) — body text: message content in the conversation, card/section titles (pair with `font_medium`/`font_bold`), main empty states ("还没有会话"), window title bar
- `text_xs` (12px) — everything secondary/meta: sidebar project & session rows, composer chip labels (model name), the model picker's list rows (name + provider subtitle, name keeps `font_medium`), timestamps, inline labels and captions, empty list placeholders
- Hierarchy within the same size comes from `font_medium`/`font_bold` and muted foreground color, not from mixing extra sizes
- Input components set their own font (theme mono size); don't override their text size

## Product reference
- Use [zed](https://github.com/zed-industries/zed) and [waku](https://github.com/egoist/waku) source code on github as a reference when you need to design a UI or build a feature
