# opencode v2 API, pinned to 2.0.16

opencode 2.0 ships its own API next to ACP: `opencode serve` is "the v2 API and
web server", an HTTP surface described by OpenAPI plus a server-sent event
stream. `openapi.json` is the server's own description (`GET /openapi.json`); it
calls itself an "Experimental HttpApi surface" at version `0.0.1`, and it does
not describe event payloads at all (`V2EventEncoded` is only "a JSON string"),
so every `turn-*.json` here is **captured traffic**: one real session's events,
kept so the hand-written slice in `src/agent/opencode/` can be checked against
what the server actually sends.

`opencode acp` on the same build still answers `protocolVersion: 1` even when
asked for 2, so "v2" here is this API, not ACP v2.

## How it was captured

A private server per run, launched the way the CLI's own `--standalone` mode
does it (read from the binary):

```bash
OPENCODE_PASSWORD=<32 random bytes, base64url> opencode serve --stdio --port 0
# stdout: {"url":"http://127.0.0.1:<port>"}
```

then `GET /api/event` held open while a session was created and prompted.
Scratch paths are replaced by `/work`, and encrypted reasoning blobs are cut.

## What the runs settled

- **Launch and lifetime.** `--stdio` prints one line with the URL and then
  serves until its stdin closes, at which point it exits by itself. `--port 0`
  gives every server its own port. Every request needs HTTP Basic auth, user
  `opencode`, password from `OPENCODE_PASSWORD`; without it everything is `401`.
- **Session ids are shared with ACP.** A `ses_…` id created over `opencode acp`
  is served by `GET /api/session/{id}`, so a restore cursor kumu stored before
  the switch still resumes.
- **A prompt is an enqueue, not a turn.** `POST /api/session/{id}/prompt`
  answers at once with the queued user message; the turn is followed on the
  event stream until `session.execution.succeeded`, `.failed` or
  `.interrupted`. `POST …/interrupt` answers `{interrupted}` and the stream then
  ends the turn with `session.execution.interrupted` (`turn-interrupt.json`).
- **Failures arrive as events.** A model that cannot be reached is not a failed
  request: the prompt is accepted and `session.execution.failed` carries
  `{error: {type, message}}` (`turn-failed-model.json`). A failed tool is
  `session.tool.failed` with the same error shape, and the turn goes on
  (`turn-tool-failed.json`).
- **Usage.** `session.usage.updated` is the *session's running total* (tokens
  and cost), not how full the context is; each `session.step.ended` /
  `.failed` carries that step's own tokens, which is what the context holds.
  The window is each model's `limit.context` in `GET /api/model`.
- **Permissions.** `permission.asked` names the tool call it gates
  (`source.id`), the action, the resources and a full diff for edits; the answer
  is `POST …/permission/{id}/reply` with `once`, `always` or `reject`
  (`turn-permission.json`, captured with a session ruleset of
  `[{action:"*",resource:"*",effect:"ask"}]`). With opencode's default rules
  the same edit runs without asking.
- **Images.** `files: [{uri: "file:///…/red.png"}]` on a prompt works, and so
  does a `data:` URI; the model answered "Red" to both (`turn-image-file.json`).
- **The catalogue loads lazily.** The first `GET /api/model` of a fresh server
  can answer an empty list; `model.updated` follows once it is ready, and the
  next read has it. Each model carries its own reasoning levels as `variants`.
- **Modes are agents.** `GET /api/agent` lists `build` and `plan` as the
  primary, non-hidden agents; `POST …/agent` switches the session's.

| File | Content |
| --- | --- |
| `openapi.json` | the server's own API description |
| `model-list.json`, `model-default.json`, `agent-list.json`, `command-list.json` | the catalogue reads a session starts from |
| `turn-shell-tool.json` | one whole turn: reasoning, a `shell` call, the reply, the settle |
| `turn-permission.json` | an edit that asked first, and the answer |
| `turn-interrupt.json` | a turn interrupted mid-tool |
| `turn-failed-model.json` | a turn whose model does not exist |
| `turn-tool-failed.json` | a tool that failed inside a turn that succeeded |
| `turn-image-file.json` | a prompt carrying an image as a `file://` URI |
