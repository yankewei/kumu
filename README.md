# kumu

## What it is

A macOS desktop shell for local coding agents: manage sessions of multiple local agents in one window (projects → session tree → conversation). Built on GPUI (gpui-kit) + Agent Client Protocol, with sessions and messages stored in local SQLite (`~/.kumu/kumu.db`).

Currently a capability demo: two windows — Home (session tree + conversation) and Settings (providers/general, dark mode). The ACP client layer is implemented but not wired into the UI yet.

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
