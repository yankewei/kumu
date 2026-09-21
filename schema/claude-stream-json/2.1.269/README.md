# Claude Code `stream-json` wire, pinned to 2.1.269

Claude Code publishes no schema generator for this protocol, so unlike
`schema/codex-app-server/` this snapshot is **captured traffic** rather than a
generated definition: one real session's lines, kept so the hand-written slice in
`src/agent/claude/protocol.rs` can be checked against what the CLI actually
sends without driving a session again.

## How it was captured

Two runs of `claude -p` in a scratch directory, with `HOME` and
`CLAUDE_CONFIG_DIR` pointed at that scratch directory so no transcript lands in
the real `~/.claude`:

```bash
# 1. a whole turn: /model, /effort, then a prompt that asks for a Write
claude -p --input-format stream-json --output-format stream-json --verbose \
  --include-partial-messages < messages.ndjson

# 2. the same, plus the flag that makes the CLI ask us for permission
claude -p --input-format stream-json --output-format stream-json --verbose \
  --include-partial-messages --permission-prompt-tool stdio < messages.ndjson
```

Run 2 also answers the `can_use_tool` request it receives, which is what proves
the round trip: the tool ran and the file appeared.

What the runs settled, none of which the official docs state outright:

- **`--permission-prompt-tool stdio` is required** for the CLI to ask the host.
  Sending the `initialize` control request alone is not enough: the same prompt
  comes back as `system/permission_denied` plus a `tool_result` with
  `is_error: true`, and the tool never runs. `--permission-prompts` defaults to
  `host` either way.
- **The control protocol's accepted subtypes**, probed directly (each of these is
  a control round trip, so none of it costs a model call):
  `initialize`, `set_permission_mode`, `set_model`, `set_max_thinking_tokens`
  and `interrupt` answer `success`; `rewind_files` is recognised but gated
  ("File rewinding is not enabled"); and **`set_sandbox`, `set_sandbox_policy`,
  `sandbox`, `set_effort`, `set_reasoning_effort`, `set_service_tier` and
  `set_context_window` are all rejected** with `Unsupported control request
  subtype`. An unknown subtype is rejected the same way, and a bad permission
  mode makes the CLI name its own accepted set:
  `acceptEdits, auto, bypassPermissions, default, dontAsk, plan`.
- **So the sandbox is not reachable per session**, only through Claude Code's own
  settings (`sandbox.*`, `/sandbox`, `--settings`), which is why kumu offers no
  sandbox axis for this agent. What the composer does expose is the one value the
  protocol *can* change, presented as the two axes it actually carries:
  - the **work mode** — `plan` (Claude Code's plan mode) against `default`, the
    wire's way of saying "not planning, use my configured mode". ACP agents
    report their own `build` / `plan` for this same chip.
  - the **access policy** — the approval ladder (`default` / `acceptEdits` /
    `auto` / `bypassPermissions`), which is the four-step menu the other clients
    in this space settle on. `dontAsk` is a real mode the CLI accepts but has no
    counterpart elsewhere, so kumu reports it without offering it; `manual` is
    not among the names the CLI lists.
- **Neither the handshake nor `system/init` reports an effort in force**, so the
  only way to know what a session will run at is the machine itself: this build
  reads `CLAUDE_CODE_EFFORT_LEVEL` (an env var or a settings `env` block — the
  CLI's own text says it "overrides effort this session") for the effort, along
  with the `effortLevel` and `modelSettings.<model>.effortLevel` keys a
  `/effort` pick saves (`src/agent/claude/settings.rs`), and
  `permissions.defaultMode` for the permission mode. The key name, the managed
  settings directory (`/Library/Application Support/ClaudeCode` on macOS,
  `/etc/claude-code` elsewhere, `managed-settings.json`) and the absence of any
  `claude config get` to ask instead all come from the 2.1.269 binary itself. With
  nothing configured, a `-p` session starts in Manual, which the CLI reports as
  `default`.
- **`initialize` is answered immediately** and its response is the richest
  catalogue: `models` (value, `resolvedModel`, `displayName`, `supportsEffort`,
  `supportedEffortLevels`), `current_permission_mode`, `commands`, `agents`,
  `account`. `system/init` only arrives with the first turn and carries less.
- **The slash commands are a real catalogue, and half of it is terminal-only.**
  `initialize`'s `commands` is what the CLI will run for this session, each entry
  with a `description` and an `argumentHint`; kumu reads the name and the hint and
  offers the list in the composer (`src/agent/claude/`, `Event::Commands`),
  because a client invokes one by sending `/name args` as an ordinary user
  message — the same path `/model` and `/effort` already take. `system/init` then
  names the subset that only works in an interactive terminal
  (`terminal_slash_commands`: `doctor`, `color`, `reload-plugins` here), and those
  are dropped once it arrives: sent over this wire they do nothing. That is the
  only signal separating the two kinds, and it is not available until the first
  turn, so a fresh session's menu lists them until then. `initialize` is also what
  the *probe* now reads them from — a handshake, no prompt, no model call.
- **No effort in force is reported anywhere.** Neither the initialize response
  nor `system/init` names the active effort, so the reasoning axis has no
  current value to show. (`--effort` can also be overridden by the user's own
  `CLAUDE_CODE_EFFORT_LEVEL` — the capture ran with it set to `max` — which is
  why that variable outranks the flag.)
- **`system/thinking_tokens` floods the stream** — one event per thinking chunk
  (674 of them in one short turn). It is not a status worth showing and must be
  ignored rather than logged.
- **`system/init` reports `permissionMode: "default"`** on a session kumu has
  configured nothing for, i.e. the value that defers to the session's own
  configuration.

## What is here

Every file is one captured message, pretty-printed (on the wire each is a single
line). Long scratch paths are replaced with `<capture>`, and `session_id`,
`uuid` and `request_id` values with placeholders; nothing else is edited. The
`models` entries in `control-initialize-response.json` are this machine's routed
catalogue (an `ANTHROPIC_AUTH_TOKEN` gateway), so they name DeepSeek models —
the *shape* is what matters, not the ids.

| File | What it is |
| --- | --- |
| `init.json` | `system/init`: cwd, session, tools, `model`, `permissionMode`, capabilities |
| `assistant-tool-use.json` | an `assistant` message whose only block is `tool_use` |
| `user-tool-result.json` | the `tool_result` block that comes back for it |
| `permission-denied.json` | `system/permission_denied`: what a request looks like with no host |
| `control-initialize.json` | the host's `initialize` control request |
| `control-initialize-response.json` | its `control_response`, including the model catalogue |
| `control-can-use-tool.json` | the CLI's `can_use_tool` request, with `permission_suggestions` |
| `control-can-use-tool-response.json` | the allow answer that makes the tool run |
| `stream-events.json` | one of each `stream_event` kind, in arrival order |
| `result.json` | the final `result` line |

## Note for the next version

`permission_suggestions` is a list of heterogeneous updates (`setMode` in the
capture, with `addRules` variants documented in the Agent SDK), so an "always
allow" answer has to model whichever kinds appear. kumu answers allow-once or
deny and leaves the suggestions unparsed.

`supportsEffort`/`supportedEffortLevels` are per model; every model in this
capture advertises the same five levels. An unsupported level clamps down rather
than failing, so reporting the union is safe — which is why the picker offers one
list.

If a later CLI gains a sandbox control request, that is the thing to re-probe
first: it would let the access menu carry a boundary as well as an approval mode,
which today it cannot.
