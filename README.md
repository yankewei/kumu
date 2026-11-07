# kumu

## What it is

A macOS desktop shell for local coding agents: manage sessions of multiple local agents in one window (projects → session tree → each session's native TUI). Built on GPUI (gpui-kit), with projects and sessions stored in local SQLite (`~/.kumu/kumu.db`).

Each session pane runs that agent's interactive TUI (or a plain `$SHELL`) inside an embedded terminal. Settings → 编码 Agent probes which agents are installed; launch paths come from `src/agent/config.rs` (`interactive`). The same agent layer still owns protocol adapters (`acp/`, `codex/`, `claude/`, `pi/`, `opencode/`) used for discovery probes over each agent's headless transport.

Two windows: Home (session tree + terminal pane) and Settings (providers/general, dark mode).

## Layout

```text
UI (src/ui/) → App (src/app/) → Agent (src/agent/) + Term (src/term/)
                     └──────→ Store (src/db.rs, SQLite)
```

- `src/agent/` — the agent registry, interactive launch helpers, protocol adapters for probes, and the `Event`/`Cmd` vocabulary. It knows nothing of the app, the store or the UI.
- `src/app/` — the `Workspace` entity: projects, sessions, live terminals, and what each agent offers (probed once, shared by both windows). It tells the views what changed through `WorkspaceEvent`s.
- `src/ui/` — drawing and interaction only: selection, expansion, and the terminal pane for what the workspace holds.
- `src/term/` — PTY + VT for agent TUIs and shell sessions.

Adding an agent that speaks an existing protocol is an entry in `src/agent/registry.rs` plus a launch builder in `src/agent/config.rs`; a new protocol is an adapter module under `src/agent/`, wired into `Config`, `spawn`, and `probe`.

## Installation

Requires Rust (`cargo`) and at least one local agent: `omp`, `opencode`, `codex`, `claude` or `pi`.

```bash
git clone git@github.com:yankewei/kumu.git
cd kumu
cargo run
```

A new session opens a login shell in the project folder. Settings → 编码 Agent shows which agents were found and re-probes them.

To run as a real macOS app — a bundle whose Dock icon survives quitting, and that can be launched from Finder or `/Applications` — build one:

```bash
packaging/make-app.sh          # → target/release/Kumu.app
packaging/make-app.sh --open   # build, then launch it
packaging/make-app.sh debug    # the faster dev binary instead of --release
```

`cargo run` starts a bare executable, so the Dock icon is only the one `src/dock_icon.rs` sets on the running process and macOS drops it on quit; a bundle carries `CFBundleIconFile`, which LaunchServices owns. The binary embeds its assets, so the bundle is the executable, `Info.plist` and `AppIcon.icns` (`packaging/`). Same PATH caveat as below: an agent installed outside Homebrew or `~/.local/bin` needs its `*_BIN` variable set for the Finder-launched app.

- `OMP_BIN` / `OPENCODE_BIN` / `CODEX_BIN` / `CLAUDE_BIN` / `PI_BIN`: path to the agent binary if it is not on PATH. For Codex, Claude Code and Pi the usual install locations are also checked, since a GUI-launched app does not inherit the shell's PATH.
- opencode needs `OPENCODE_API_KEY` (env first, otherwise read automatically from `~/.local/share/opencode/auth.json`); omp needs no setup.
- Codex must be signed in first: run `codex login` once (kumu says so plainly if it is missing). Nothing extra gets installed — `codex` is a single native binary, so no Node runtime is involved.
- `KUMU_DEBUG=1 cargo run`: print agent events to stderr for headless verification.
