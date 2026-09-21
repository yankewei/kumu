# AGENTS.md
A client that drives multiple local coding agents, each over its own protocol, behind one backend layer.

## Tech stack
- gpui-kit, the entire UI: GPUI rendering engine (wgpu drawing, text layout), declarative element tree, component library (Button/Input/Switch/TitleBar/Root), theme system (ThemeRegistry + semantic tokens), multi-window management. Hover labels on app elements go through an overlay of the app's own (src/ui/tooltip.rs, mounted by src/ui/layers.rs::`window_layers`) because the one `Root` mounts is crate-private — the tooltip surface and the overlay are both public API, the trait that asks the overlay for a tooltip is not
- agent-client-protocol, the ACP backend: stdio subprocess handshake (initialize/session/new/prompt), permission requests, config option parsing, all in src/agent/acp/. Serves opencode and Oh My Pi (`omp acp`) — the latter a fork of Pi, and not the same agent as `pi` below
- codex app-server, the Codex backend: newline-delimited JSON-RPC over stdio (initialize, thread/start, turn/start, approvals), hand-written in src/agent/codex/ because the wire omits the `jsonrpc` field that a JSON-RPC crate requires. No extra crate: it uses tokio + serde_json, which were already dependencies
- claude -p, the Claude Code backend: newline-delimited `stream-json` over stdio — the transport the Claude Agent SDK's `query()` drives, the SDK being a wrapper around these flags rather than a capability of its own. One long-lived process per session, in src/agent/claude/, with the wire pinned to captured traffic in schema/claude-stream-json/. `--permission-prompt-tool stdio` is undocumented but is what makes the CLI ask kumu about a tool call instead of deciding alone
- pi --mode rpc, the Pi backend: Pi's own newline-delimited command/reply protocol over stdio (string command ids; `{"type":"response","id","success","data"}` replies; everything else on stdout an unsolicited event), in src/agent/pi/, with the wire pinned to captured traffic in schema/pi-rpc/. Neither JSON-RPC nor ACP, and with no permission system at all: a prompt is acknowledged before it runs and only `agent_settled` ends a turn, so a turn is followed on the event stream rather than by waiting on the reply
- merman (pinned to an exact alpha), Mermaid fences in replies, src/ui/home/markdown_mermaid.rs: a pure-Rust port of Mermaid that lays out and writes SVG in process — no browser, CLI or subprocess — rendered through its `resvg_safe` pipeline because GPUI rasterises with resvg, which cannot draw `foreignObject`. Colours come from the app theme as merman `ThemeRole`s. Only `svg` + `layout-cytoscape` are enabled: ELK is EPL-2.0, so layouts or presets that ask for it fail with `MissingCapability`. Zed uses the same crate
- rusqlite, persistence layer src/db.rs: projects/sessions/messages tables, bundles SQLite at compile time, no system library needed
- tokio, the dedicated async runtime thread behind src/agent/: drives async stdio read/write and protocol request/notification streams; UI→backend uses its unbounded channel
- serde + serde_json, everywhere a payload is parsed: theme JSON into ThemeRegistry, the Codex protocol types, the frames the hand-written backends read, and opencode's auth.json for the ACP backend's API key

## Constraints
1. Build the UI with gpui-kit components first
2. Each backend speaks its agent's own protocol over a stdio subprocess — ACP for opencode/omp, `codex app-server` for codex, `claude -p` stream-json for claude, `pi --mode rpc` for pi — and no UI code may branch on which one is in use. Every backend converges on `agent::Event` / `agent::Cmd` (src/agent/event.rs), which is the only vocabulary the UI knows; protocol types stop at the backend boundary. Which protocol an agent speaks is decided in src/agent/config.rs (`Config::for_agent`) and dispatched in `spawn` and `probe`, so adding an agent is a registry entry in src/agent/registry.rs plus a builder there — the two exhaustive `Backend` matches are the compiler's to-do list
3. 在写测试的时候只需要写有必要的测试，没有必要覆盖非常边缘的可能性

## Type scale
Stay on the two-step rem scale; no custom `text_size(px(...))` or `text_base`+ in app code:
- `text_sm` (14px) — body text: message content in the conversation, card/section titles (pair with `font_medium`/`font_bold`), main empty states ("还没有会话"), window title bar
- `text_xs` (12px) — everything secondary/meta: sidebar project & session rows, composer chip labels (model name), the model picker's list rows (name + provider subtitle, name keeps `font_medium`), timestamps, inline labels and captions, empty list placeholders
- Hierarchy within the same size comes from `font_medium`/`font_bold` and muted foreground color, not from mixing extra sizes
- Input components set their own font (theme mono size); don't override their text size

## Product reference
- Use [zed](https://github.com/zed-industries/zed) and [waku](https://github.com/egoist/waku) source code on github as a reference when you need to design a UI or build a feature
