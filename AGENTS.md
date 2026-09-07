# AGENTS.md

## What this is

macOS desktop shell for local coding agents, built with gpui-kit 0.6 (GPUI) + agent-client-protocol 2.1. Capability demo: two windows — Home (projects → sessions tree + conversation) and Settings — backed by local SQLite. ACP layer exists but is not wired into the UI yet.

## Commands

```bash
cargo run                       # default agent: omp
KUMU_AGENT=opencode cargo run   # opt-in agent
KUMU_DEBUG=1 cargo run          # echoes ACP events to stderr (headless verification)
```

- No tests exist. Verification = `cargo check`/`cargo build` plus `KUMU_DEBUG=1` stderr output (expected: `[acp] log: spawning…`, `session ready: ses_…`).
- No CI, no lint config. Don't invent test commands.

## Environment

- `KUMU_AGENT` = `omp` (default) | `opencode`; `OMP_BIN`/`OPENCODE_BIN` override binaries.
- `OPENCODE_API_KEY`: env first, else kumu reads the RC-AI-Gateway key from `~/.local/share/opencode/auth.json` (so GUI launches work). omp needs no injection.
- `KUMU_AUTO_PROMPT`: send one prompt automatically after the session opens (headless debugging).

## Module boundaries

```
src/main.rs   entry only: init, themes, open Home window
src/ui/       GPUI views, one module per window (home.rs, settings.rs) + widgets.rs
src/theme.rs  themes/*.json → ThemeRegistry
src/agents.rs shared agent registry (AGENTS)
src/db.rs     SQLite persistence (~/.kumu/kumu.db)
src/acp.rs    ACP client thread; compiled via `#[allow(dead_code)] mod acp;` — not wired yet
```

- `db.rs` and `acp.rs` must stay gpui-free. acp.rs talks to the UI only through its `Event` enum + mpsc channels.
- Cross-window: windows are separate view entities wrapped in `Root::new(view, window, cx)`; Home holds `Option<WindowHandle<Root>>` for the settings window (reuse-and-activate pattern).
- `Theme` is an App-level global: `Theme::change` resyncs every window (settings dark-mode switch relies on this).

## gpui-kit fork quirks (breaks assumptions from upstream gpui docs)

- kumu uses gpui-kit 0.6 → the `gpui-pre-0.3.3` fork, not `gpui` 0.2.x from crates.io. Check the fork's source in `~/.cargo/registry` before using a styled helper.
- Spacing helpers exist only on the 0–128 ramp; tailwind named sizes like `max_w_xl` do NOT exist (use e.g. `max_w_128()` = 32rem).
- rem base is fixed at 16px: `text_sm` = 14px, `text_base` = 16px.
- System UI font is the literal name `.SystemUIFont` (set in themes/*.json under `"font.family"`).
- New windows need `TitleBar::window_options()` (transparent title bar + traffic lights); the settings window sets `window_min_size` equal to its initial size.

## Style conventions

- Colors only via `cx.theme()` semantic tokens (`theme.muted_foreground`, `sidebar_*`…). No raw hex/Hsla literals in app code — edit `themes/*.json` instead.
- Layout in rem helpers (`w_64`, `px_8`, `gap_3`); px only for physical boundaries (`h_px` hairlines, window chrome).
- UI strings are Chinese (e.g. 新建会话, 服务商); code identifiers/comments in English.

## Docs

`README.md` (architecture, run matrix, verified ACP findings) is kept current — update it when moving modules or changing the agent wiring.
