# Pi RPC wire, pinned to 0.85.1

Pi documents this protocol itself (<https://pi.dev/docs/latest/rpc>), but the
documentation leaves several things unsaid that the backend in
`src/agent/pi/` has to know. This snapshot is **captured traffic** from a real
`pi --mode rpc` process, kept so those questions can be re-checked without
driving a session again — the same role
[`schema/claude-stream-json/`](../claude-stream-json/2.1.269/README.md) plays for
Claude Code.

## How it was captured

Three runs of `pi --mode rpc` in a scratch directory, driven by a small script
that writes commands on stdin and records every stdout line:

```bash
pi --mode rpc --approve --no-session --no-skills \
  --no-prompt-templates --no-context-files
```

Exactly the argv the backend's probe used at the time of capture. (`--no-session`
also means nothing lands in the real `~/.pi` history.) Every flag was checked
against `pi --help` for 0.85.1 — Pi rejects an unknown flag outright, so the list
cannot be padded.

**The probe's argv has since changed**: it no longer passes `--no-skills` or
`--no-prompt-templates`, because a probe is now asked for the session's *commands*
as well (`get_commands`), and Pi reports skills and prompt templates as exactly
those. `--no-session` and `--no-context-files` still stand. The captures above
were not re-run, so the command lists in them are trimmed by those two flags —
the `get_commands` shape documented below is Pi's own documentation, not a
captured frame.

The runs were:

1. `get_state`, `get_available_models`, `get_available_thinking_levels` — the
   handshake, no model call.
2. `set_model` to `openai-codex/gpt-5.5`, then a prompt asking for a `bash`
   call: a whole turn, streamed, with a tool.
3. A prompt on a provider that could not serve it, twice over (an unpaid
   balance, and a model the account may not use).

## What the runs settled

None of this is spelled out in Pi's documentation, and the backend's shape
follows from it:

- **A reply is an acceptance, not a result.** `{"id":"4","type":"prompt",…}`
  answers `success: true` immediately; the model call happens afterwards. So the
  turn has to be followed on the event stream, and the reply alone says nothing
  about whether it worked.
- **`agent_end` is not the end of a turn.** `turn-stream.json` shows one prompt
  producing `turn_start`/`turn_end` **twice** — the tool call ends one turn and
  its continuation begins another — with a single `agent_start` and a single
  `agent_settled` around them. Only `agent_settled` is safe to treat as "this
  prompt is done", which is what the backend waits on.
- **A failed run is not an error event.** Both refusals finished the assistant
  message with `stopReason: "error"` and the provider's own words in
  `errorMessage`, then `turn_end`, `agent_end` and a normal `agent_settled`.
  There is no error frame to catch, so the backend reads the failure off the
  message — and reports it only when the turn settles, since Pi may retry first.
- **A tool's arguments exist only on the start frame.** `tool_execution_start`
  carries `args`; `tool_execution_update` and `tool_execution_end` do not, so the
  row's title can only be read once — which is what `Event::ToolCall`'s "empty
  means untouched" contract wants. The tool names and argument keys are Pi's own:
  `bash` takes `command`, `read` takes `path`.
- **`partialResult` is the accumulated output, not a delta.** The first update
  carries an empty `content` array and the second carries the whole output, so a
  row can replace its body on each update rather than appending.
- **`message_update` also carries the tool call.** `toolcall_start` / `_delta` /
  `_end` arrive *before* `tool_execution_*`, with the same call and its arguments
  streamed character by character. The backend ignores them and renders
  `tool_execution_*` only; handling both would put two rows on every tool call.
- **The thinking levels come from `get_available_thinking_levels`, not from the
  model.** `get-state.json` shows a per-model `thinkingLevelMap` that names only
  the levels that are *remapped* for that model (`xhigh`, `minimal` here) — it is
  a mapping table, not a menu. The advertised list is the complete one, and it
  includes `off` even for a model with no reasoning.
- **The commands are a documented request, not a captured one.** `get_commands`
  answers with `{name, description, source}` per entry (`source` is `extension`,
  `prompt` or `skill`; a skill's name carries its `skill:` prefix). Pi's
  documentation says these are invocable "via the `prompt` command by prefixing
  with `/`", and that the built-in TUI commands (`/settings`, `/hotkeys`, …) are
  deliberately **not** included because sending them over RPC would not execute —
  which is what makes this list exactly the set worth offering. kumu reads it as
  the composer's `/` menu (`src/agent/pi/`, `Event::Commands`). **Not verified
  against a captured frame**: the captures above ran with `--no-skills` and
  `--no-prompt-templates`, so their command lists are trimmed.
- **The catalogue is per machine and dynamic.** This machine reports 105 models
  across four providers, with `provider` on each entry, which is why a model is
  addressed as `provider/id` and why the probe has to ask rather than use a
  fixed vocabulary.
- **There is no `ready` frame**, unlike Oh My Pi's protocol, and frames can
  arrive before any command is sent: this machine's extensions emit
  `extension_ui_request`/`setStatus` while the process is still starting. So the
  reader must tolerate events at any time, including before the handshake.
- **stderr stays empty** in every run, including both failures — the reason
  arrives on the protocol, not on stderr.
- **Under `--no-session`, `get_state` reports `sessionId` but no `sessionFile`.**
  The file path is what Pi needs to resume, which is why the backend does not
  treat the session id as a resumable cursor.

## What is here

Each file is the wire content pretty-printed (on the wire every frame is one
line). The `get_available_models` catalogue is trimmed to two models per
provider because its *shape* is the point, not this machine's 105 entries; the
long `ls` output inside the captured tool result is replaced with a
`<N chars of output>` marker for the same reason. Nothing else is edited beyond
the session id placeholder.

| File | What it is |
| --- | --- |
| `get-state.json` | the `data` of a `get_state` reply: model, thinking level, session id, queue modes |
| `get-available-models.json` | the `data` of `get_available_models`, trimmed: `{id, name, provider, …}` per model |
| `get-available-thinking-levels.json` | the `data` of `get_available_thinking_levels`: the seven levels |
| `turn-stream.json` | one whole prompt: two turns, a `bash` call with its output, and the settle |
| `turn-refused.json` | a prompt the provider refused: the message that reports it, and the normal settle |
| `extension-ui-request.json` | the `extension_ui_request` kinds this machine's extensions emit |

## Left unpinned

- **The extension dialog methods** (`select`, `confirm`, `input`, `editor`) were
  never observed: this machine's extensions only emit fire-and-forget ones
  (`setStatus`). kumu answers a dialog with `cancelled`, which is what Pi's
  documentation describes a client doing when it cannot show the dialog — but no
  run here proved an extension actually blocks on one, so if a turn is ever seen
  parking behind an extension, this is the first thing to re-probe.
- **Steering** (`steer`, and `streamingBehavior` on `prompt`) is not used: the UI
  refuses to send a prompt while a turn is in flight, and the backend parks its
  command loop until the turn settles, so a second prompt cannot arise.
- **Rewind and branch**: Pi's `get_fork_messages` / `fork` / `clone` are not wired.
  Session recovery now saves `get_state.sessionFile` and uses `switch_session`
  before sending the next prompt. That command is documented and covered by a
  local protocol fixture, but these original captures did not exercise it.
- **The 1 MiB stdout ceiling** Pi's documentation mentions for oversized frames
  was not hit; the largest frame observed was the model catalogue at ~40 KB.

## Note for the next version

`stopReason` is the field to check first on any new release. Both failures here
were reported through it (`error` with `errorMessage`), and a retry is announced
separately by `auto_retry_end` — a build that reported failures any other way
would show an empty answer as a success.

Pi 0.85.1's help advertises `--offline` / `PI_OFFLINE=1` for startup network
operations; the `PI_SKIP_VERSION_CHECK=1` variable other clients set for Pi is
not mentioned by this build, so kumu sets neither. If a startup update check ever
turns out to delay the RPC loop, `--offline` is the flag to reach for.
