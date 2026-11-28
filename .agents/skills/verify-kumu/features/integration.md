# Agent integration & hook transport

Agent integration inspects and manages notification hooks, MCP registration, and embedded skills for supported coding agents, and delivers OSC 777 status/attention sequences over PTYs.

## Sub-features

- `integration-status` reports per-agent detection, notification, MCP, and skills registration state as JSON.
- `hook-transport-pty` verifies that `--kumu-notify` emits valid OSC 777 sequences for Claude (stdout JSON) and Codex/Grok/Cursor/Antigravity (direct write to `$KUMU_TTY` without a controlling terminal).

## How to get to it (user POV)

- Click one-click connect in Step 1 of the onboarding wizard or in **Settings → Agent 接入**.
- Run `kumu integration status [--json]`, `kumu integration install [AGENT]`, or `kumu integration uninstall [AGENT]` in a terminal.

## Driving it with kumu-verify.sh

Preconditions:

- `./target/debug/kumu` is built.
- `kumu-verify.sh doctor` succeeds (for isolated `HOME` status checks).

- **Inspect isolated integration status.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli integration status --json | tee "$KUMU_VERIFY_EVIDENCE/integration/status.json"`. Exit code `0`; stdout JSON lists supported agents (`claude`, `codex`, `opencode`, `agy`, `cursor`, `grok`, `pi`, `omp`) with `installed`, `notify`, `mcp`, and `skills` fields reflecting the empty isolated `HOME`.
- **Run real-PTY hook transport suite.** Run `python3 scripts/test-agent-integration.py 2>&1 | tee "$KUMU_VERIFY_EVIDENCE/integration/hook-transport.txt"`. Exit code `0`; all test cases pass, proving `setsid()` hook processes emit well-formed `\x1b]777;notify;<agent>;...\x07` sequences.

## Gotchas

- Never run `kumu integration install` or `uninstall` without `HOME="$KUMU_VERIFY_HOME"`, or it will modify your real `~/.claude`, `~/.codex`, `~/.gemini`, and `~/.agents` configs.
- `scripts/test-agent-integration.py` invokes `./target/debug/kumu` directly; rebuild with `cargo build` after Rust edits before running it.
