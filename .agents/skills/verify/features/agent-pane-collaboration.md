# Agent pane collaboration

Kumu lets an agent or user split a sibling pane running a supported coding agent (`claude`, `codex`, `opencode`, `agy`, `cursor`, `pi`, `omp`, `grok`), send prompts to that agent pane, wait for a terminal status transition, and close the temporary pane when finished.

## Sub-features

- `collab-split` opens a new agent pane to the `right` or `down` while keeping focus on the original pane.
- `collab-send` pastes follow-up instructions into a live agent pane (with or without `--no-enter`).
- `collab-wait` blocks until the target agent reaches `waiting`, `completed`, `failed`, or exits.
- `collab-close` closes the temporary pane without closing the caller unless `--self` is passed.

## How to get to it (user POV)

- Click the split/new-agent button in a pane header or press `⌘D` / `⌘⇧D` and launch an agent in the new pane.
- Run `"$KUMU_APP_BINARY" pane split right|down --agent <id> [--prompt "..."]`, `pane send`, `pane wait`, and `pane close` from a Kumu pane.

## Driving it with kumu-verify

Preconditions:

- An isolated Kumu instance `<RUN_ID>` is running and `kumu-verify doctor "<RUN_ID>"` passes.
- At least one supported coding agent CLI (for example `codex`, `claude`, `opencode`, or `agy`) is installed on `PATH` and logged in. If none is installed, stop and report `agentNotInstalled` as an unmet precondition.

- **Split an agent pane.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane split right --agent <AGENT_ID> --json`. Exit code `0`; JSON returns the new pane's numeric `<NEW_ID>`, and `panes --json` now shows two live panes with the original pane still `"focused": true`.
- **Verify agent pane text.** Wait briefly for the TUI to initialize, then run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane read <NEW_ID> --tail 30 --json`. Exit code `0`; output shows `"agent": "<AGENT_ID>"` and non-empty rendered screen text.
- **Send a draft without submitting.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane send <NEW_ID> "status check" --no-enter --json`. Exit code `0`; `pane read <NEW_ID>` shows the drafted text without triggering an unintended model call.
- **Close the temporary pane.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane close <NEW_ID> --json` followed by `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- panes --json`. Exit code `0`; `<NEW_ID>` is no longer in the pane list.
- **Proof.** Save the split, read, send, close, and final `panes --json` outputs into `/tmp/kumu-verify-evidence/<RUN_ID>/agent-pane-collaboration/`.

## Gotchas

- `pane split --prompt "..."` waits up to ~12 seconds for the agent TUI to draw its prompt; if the agent is slow to boot or shows a first-run login/trust dialog, it returns `promptTimeout` while leaving the pane open.
- Using `--no-enter` lets you prove `pane send` reaches an agent pane without spending API tokens or waiting on an LLM response; when you specifically need to verify `pane wait`, submit a trivial prompt and pass `--timeout 60`.
- `pane close` refuses to close the caller pane (`refuseSelf`) unless `--self` is passed.
