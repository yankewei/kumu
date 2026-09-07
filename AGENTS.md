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

## Product reference
- Use [zed](https://github.com/zed-industries/zed) source code on github as a reference when you need to design a UI
