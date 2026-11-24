# Agent notifications

Coding agents report task completion, permission prompts, and input waits back to Kumu through `kumu --kumu-notify <agent> <event>`, which either returns a JSON `terminalSequence` on stdout (Claude Code) or writes an OSC 777 escape sequence directly to `KUMU_TTY` (Codex, Grok Build, Cursor CLI, Antigravity CLI) even when the hook process has no controlling terminal.

## Sub-features

- `notify-claude-stdout` emits a JSON payload containing `\x1b]777;notify;claude;...\x07` for `complete`, `permission`, and `input`.
- `notify-tty-agents` writes OSC 777 sequences to `KUMU_TTY` from a detached session (`setsid`) for `codex`, `grok`, `cursor`, and `agy`.
- `notify-custom-launcher` preserves arbitrary arguments with spaces when a custom launcher script invokes `$KUMU_APP_BINARY --kumu-notify`.

## How to get to it (user POV)

- Enable agent notifications in **Settings → Providers → Coding Agents** (or run an agent inside a Kumu pane with hooks installed); Kumu updates the sidebar/pane status capsule and posts a macOS notification banner (when launched as a `.app` bundle).
- Invoke `"$KUMU_APP_BINARY" --kumu-notify <agent> <complete|permission|input>` directly as agent hooks do.

## Driving it with kumu-verify

Preconditions:

- `target/debug/kumu` is built (`cargo build`).
- The evidence directory `/tmp/kumu-verify-evidence/<RUN_ID>/agent-notifications` exists.

- **Run detached PTY hook transport suite.** Run `python3 scripts/test-agent-integration.py -v`. Exit code `0`; all subtests for `claude`, `codex`, `grok`, `cursor`, and `agy` pass against `target/debug/kumu`.
- **Direct CLI check (Claude JSON path).** Run `env -u KUMU_TTY -u KUMU_SESSION_TOKEN KUMU_TERMINAL=1 ./target/debug/kumu --kumu-notify claude complete`. Exit code `0`; stdout is JSON containing `"terminalSequence"` starting with `\u001b]777;notify;claude;` and ending with `\u0007`.
- **Proof.** Save the `test-agent-integration.py -v` transcript and direct `--kumu-notify` output into `/tmp/kumu-verify-evidence/<RUN_ID>/agent-notifications/`.

## Gotchas

- macOS system notification banners require launching a signed/bundled `.app` (`packaging/make-app.sh`), not the bare `target/debug/kumu` binary. Bare binary runs prove the hook transport and in-app status path, not macOS Notification Center banners.
- Always unset `KUMU_SESSION_TOKEN` when running standalone hook tests inside a real Kumu pane, or the binary will append the host pane's session token to the emitted sequence.
