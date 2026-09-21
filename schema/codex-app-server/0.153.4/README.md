# Codex `app-server` schema, pinned to 0.153.4

The authoritative schema for the protocol the Codex backend speaks, vendored so
the hand-written slice in `src/agent/codex/protocol.rs` can be checked against
it without a Codex install. The generator is Codex's own:

```bash
codex app-server generate-json-schema --out DIR
```

Everything here came from that command against **codex-cli 0.153.4**. To refresh
it, generate a new version's dump into a temp directory and diff it against
this one; a new release gets a sibling directory rather than overwriting this
one, since a Codex upgrade is what the pin is about.

## What is here

The generator emits one file per type plus two aggregates. Only the aggregates
and the four dispatch files are kept: the per-type files are extractions of the
same definitions, so dropping them loses nothing and keeps ~280 files out of the
tree.

| File | What it is |
| --- | --- |
| `codex_app_server_protocol.schemas.json` | the v1 definitions (83), including `InitializeParams`/`InitializeResponse` |
| `codex_app_server_protocol.v2.schemas.json` | the v2 definitions (622), including `ThreadStartParams`, `TurnStartParams`, `ModelListResponse` |
| `ClientRequest.json` | client -> server methods, mapped to their params/results |
| `ServerRequest.json` | server -> client requests (the approval prompts) |
| `ServerNotification.json` | server -> client notifications |
| `ClientNotification.json` | the one client notification (`initialized`) |

The handshake is v1 while everything kumu actually drives a session with is v2,
so both aggregates are needed.

## Note for the next version

`turn/start.effort` is the only way to set a reasoning effort — `thread/start`
does not accept one — and `thread/start`'s response echoes the effective value
back. `model/list` advertises `defaultReasoningEffort` and
`supportedReasoningEfforts` per model. These are the shapes `protocol.rs` and
`convert.rs` rely on.

app-server has no slash commands: those are Codex's TUI's own (`SlashCommand`
in `codex-rs/tui`), which calls methods such as `thread/compact/start` itself.
What the composer offers for Codex is therefore a few built-ins the backend
maps itself — `/compact` → `thread/compact/start` (an acknowledgement only; the
turn announces itself with `turn/started` and shows as a `contextCompaction`
item), `/review*` → `review/start` with `delivery: "inline"` (the findings
arrive as a completed `agentMessage` that never streamed), `/rename` →
`thread/name/set`, `/init` → the TUI's own prompt — followed by `skills/list` — per entry,
`skills[].name`/`path`/`enabled` — refreshed on `skills/changed`. A skill is
invoked the way the TUI does it: the text spells it `$name`, and a
`{"type":"skill","name","path"}` input item beside it names it exactly (Codex
prefers the structured item over the text mention). Checked against a live
`skills/list` from codex-cli 0.154.0; the turn-side shape is the schema's
`SkillUserInput`, not a captured frame.
