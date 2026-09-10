# kumu

## What it is

A macOS desktop shell for local coding agents: manage sessions of multiple local agents in one window (projects → session tree → conversation). Built on GPUI (gpui-kit) + Agent Client Protocol, with sessions and messages stored in local SQLite (`~/.kumu/kumu.db`).

Two windows: Home (session tree + conversation) and Settings (providers/general, dark mode). ACP is wired into the UI — a prompt streams the agent's thinking, tool calls and markdown reply into the conversation, and the composer picks the agent's model. When an agent asks for authorization (`session/request_permission`), the choices appear on the tool row they belong to and are never auto-approved; the answer is passed back as the agent's own option id.

## Installation

Requires Rust (`cargo`) and a local agent binary (default `omp`, or `opencode`).

```bash
git clone git@github.com:yankewei/kumu.git
cd kumu
cargo run                        # uses omp by default
KUMU_AGENT=opencode cargo run    # or opencode
```

- `OMP_BIN` / `OPENCODE_BIN`: path to the agent binary if it is not on PATH.
- opencode needs `OPENCODE_API_KEY` (env first, otherwise read automatically from `~/.local/share/opencode/auth.json`); omp needs no setup.
- `KUMU_DEBUG=1 cargo run`: print ACP events to stderr for headless verification.
