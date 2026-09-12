# AGENTS.md
A client that drives multiple local coding agents, each over its own protocol, behind one backend layer.

## Tech stack
- gpui-kit, the entire UI: GPUI rendering engine (wgpu drawing, text layout), declarative element tree, component library (Button/Input/Switch/TitleBar/Root), theme system (ThemeRegistry + semantic tokens), multi-window management
- agent-client-protocol, the ACP backend: stdio subprocess handshake (initialize/session/new/prompt), permission requests, config option parsing, all in src/backend/acp/
- codex app-server, the Codex backend: newline-delimited JSON-RPC over stdio (initialize, thread/start, turn/start, approvals), hand-written in src/backend/codex/ because the wire omits the `jsonrpc` field that a JSON-RPC crate requires. No extra crate: it uses tokio + serde_json, which were already dependencies
- rusqlite, persistence layer src/db.rs: projects/sessions/messages tables, bundles SQLite at compile time, no system library needed
- tokio, the dedicated async runtime thread behind src/backend/: drives async stdio read/write and protocol request/notification streams; UI→backend uses its unbounded channel
- serde + serde_json, three places: parsing theme JSON into ThemeRegistry; the Codex protocol types; the ACP backend reads opencode's auth.json for the API key

## Constraints
1. Build the UI with gpui-kit components first
2. Each backend speaks its agent's own protocol over a stdio subprocess — ACP for opencode/omp, `codex app-server` for codex — and no UI code may branch on which one is in use. Every backend converges on `backend::Event` / `backend::Cmd` (src/backend/event.rs), which is the only vocabulary the UI knows; protocol types stop at the backend boundary

## Type scale
Stay on the two-step rem scale; no custom `text_size(px(...))` or `text_base`+ in app code:
- `text_sm` (14px) — body text: message content in the conversation, card/section titles (pair with `font_medium`/`font_bold`), main empty states ("还没有会话"), window title bar
- `text_xs` (12px) — everything secondary/meta: sidebar project & session rows, composer chip labels (model name), the model picker's list rows (name + provider subtitle, name keeps `font_medium`), timestamps, inline labels and captions, empty list placeholders
- Hierarchy within the same size comes from `font_medium`/`font_bold` and muted foreground color, not from mixing extra sizes
- Input components set their own font (theme mono size); don't override their text size

## Product reference
- Use [zed](https://github.com/zed-industries/zed) and [waku](https://github.com/egoist/waku) source code on github as a reference when you need to design a UI or build a feature
