# kumu

A macOS desktop app idea: manage multiple local coding agents (ACP) in one UI.
Built with [GPUI](https://gpui-kit.com) (gpui-kit 0.6) + the official
[Agent Client Protocol Rust SDK](https://agentclientprotocol.com/libraries/rust).

> Status: **capability demo (M0–M3 proof)**, protocol v1 only, single agent
> (Opencode), single session, auto-approve permissions.

## What the demo proves

- A GPUI window talks to `opencode acp` over stdio (ACP v1).
- Full prompt round-trip works: initialize → session/new → prompt → streamed
  `agent_message_chunk` → idle stop reason.
- gpui (main thread) and the ACP connection (dedicated tokio thread) stay
  decoupled through two channels:
  - ACP → UI: `std::sync::mpsc`, drained by a gpui poll task.
  - UI → ACP: `tokio::sync::mpsc::UnboundedSender` (non-blocking send).

## Architecture

```
┌─ UI (gpui/gpui-kit, main thread)      src/main.rs
│    input box + send button + message list (status/log/agent/user rows)
├─ poll task: drain ACP→UI mpsc, update entity on main thread
├─ ACP thread (dedicated tokio runtime) src/acp.rs
│    agent-client-protocol Client role → `opencode acp` subprocess (stdio)
│    on_receive_notification: agent_message_chunk → Event::Agent
│    on_receive_request:      permission → auto-approve allow_* option
└─ config                          agent command/args/env + cwd
```

## Run

```bash
cargo run
```

Requires `opencode` on PATH (or `OPENCODE_BIN`). The agent needs a reachable
model; kumu injects `OPENCODE_API_KEY` (env first, else the RC-AI-Gateway key
opencode already stored in `~/.local/share/opencode/auth.json`) so it works when
launched from a GUI.

Headless verification:

```bash
KUMU_DEBUG=1 KUMU_AUTO_PROMPT="Reply with only the word: OK" cargo run
```

Expected on stderr:

```
[acp] log: spawning: .../opencode acp
[acp] log: initialized: agent=Some("OpenCode")
[acp] log: session ready: ses_...
[acp] agent: OK
[acp] done: EndTurn
```

## Verified findings

- Opencode 1.18 supports ACP v1 (`initialize`/`session/new`/`session/prompt`).
- Its default model routing hit the built-in AI Gateway and failed
  (`AI Gateway Error`) until `OPENCODE_API_KEY` was injected — provider config
  uses `{env:OPENCODE_API_KEY}`.
- v1 `RequestPermissionRequest` has **no** `title`/`subject` (that's v2); it
  carries `tool_call` + `options`.

## Next steps (not yet implemented)

- Cancel (`session/cancel`) + stop-reason-driven UI.
- Permission prompts surfaced in the UI instead of auto-approve.
- Plan / tool-call / usage rendering; real diff display.
- Multiple sessions on one opencode connection (one `session/new` per project
  `cwd`) — the stated core use case; verify opencode's single-process
  multi-session concurrency first.
- Multiple agents; protocol v2 behind a feature flag.
