# kumu

## What it is

A macOS desktop shell for local coding agents: manage sessions of multiple local agents in one window (projects → session tree → conversation). Built on GPUI (gpui-kit), with sessions and messages stored in local SQLite (`~/.kumu/kumu.db`).

Each agent is driven over its own protocol on a stdio subprocess — [Agent Client Protocol](https://agentclientprotocol.com/) for opencode and Oh My Pi (`omp`), `codex app-server` for Codex, `claude -p` stream-json for Claude Code, and `pi --mode rpc` for Pi — behind one agent layer that reduces them all to the same events. Nothing above that layer knows which agent is talking.

Two windows: Home (session tree + conversation) and Settings (providers/general, dark mode). A prompt streams the agent's thinking, tool calls and markdown reply into the conversation, and the composer picks the session's agent and model, along with whichever of reasoning effort, access, mode and sandbox that agent offers. When an agent asks for authorization, the choices appear in a card above the composer and are never auto-approved; the answer is passed back as the agent's own option id, and the tool row records what was chosen.

## Layout

```text
UI (src/ui/) → App (src/app/) → Agent (src/agent/) ── stdio ── agent process
                     └──────→ Store (src/db.rs, SQLite)
```

- `src/agent/` — the agent registry, one backend per protocol, and the `Event`/`Cmd` vocabulary they all speak. It knows nothing of the app, the store or the UI.
- `src/app/` — the `Workspace` entity: projects, sessions, each session's connection and live turn, permission decisions, picks, and what each agent offers (probed once, shared by both windows). It tells the views what changed through `WorkspaceEvent`s, so a streamed chunk repaints only the conversation.
- `src/ui/` — drawing and interaction only: selection, expansion, input, and the render caches (parsed markdown) for what the workspace holds.

Adding an agent that speaks an existing protocol is an entry in `src/agent/registry.rs` plus a launch builder in `src/agent/config.rs`; a new protocol is a backend under `src/agent/`, and the exhaustive `Backend` matches list where to wire it in.

## Session recovery

Kumu saves each agent's restore cursor alongside the local conversation. After restarting the app or reconnecting, sending the next message reopens that agent conversation before starting a new turn: Codex uses `thread/resume`, Claude Code uses `--resume`, Pi loads its saved session file, and ACP agents use `session/load` when they advertise support. The existing local messages stay visible without duplicating ACP's history replay. Recovery restores context; it does not restart an interrupted command.

The agent's own session storage must still exist. If recovery fails or the current ACP agent does not support loading, Kumu reports the failure and does not send the prompt into a fresh conversation. Create a new Kumu session to start over. Conversations created before restore cursors were saved cannot recover their original agent context; Kumu shows a notice when starting a new agent conversation for that history. The SQLite schema upgrades automatically without removing messages.

## Installation

Requires Rust (`cargo`) and at least one local agent: `omp`, `opencode`, `codex`, `claude` or `pi`.

```bash
git clone git@github.com:yankewei/kumu.git
cd kumu
cargo run
```

A new session starts on `omp`; pick another agent (and its model) from the composer's model chip before the first message. Settings → 编码 Agent shows which agents were found and re-probes them.

- `OMP_BIN` / `OPENCODE_BIN` / `CODEX_BIN` / `CLAUDE_BIN` / `PI_BIN`: path to the agent binary if it is not on PATH. For Codex, Claude Code and Pi the usual install locations are also checked, since a GUI-launched app does not inherit the shell's PATH.
- opencode needs `OPENCODE_API_KEY` (env first, otherwise read automatically from `~/.local/share/opencode/auth.json`); omp needs no setup.
- Codex must be signed in first: run `codex login` once (kumu says so plainly if it is missing). Nothing extra gets installed — `codex` is a single native binary, so no Node runtime is involved.
- `KUMU_DEBUG=1 cargo run`: print agent events to stderr for headless verification.
