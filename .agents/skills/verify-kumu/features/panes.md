# Pane control & inspection

Pane control lets a user or coding agent list workspace panes, read a pane's terminal output and scrollback, split a new agent pane, and close panes over Kumu's local Unix control socket.

## Sub-features

- `panes-list` returns every active pane with its ID, project, tab, focus state, and dimensions.
- `pane-read` captures the visible screen and requested scrollback history of a pane.
- `pane-split-agent` splits a pane right or down and launches a configured coding agent.
- `pane-close` closes a target pane while refusing to close the caller pane unless `--self` is passed.

## How to get to it (user POV)

- Run `kumu panes [--json]` or `kumu pane <read|scroll|split|send|wait|close> ...` from inside a Kumu terminal pane (or via the built-in MCP server).
- Split or close panes interactively in the window with `⌘D` (split right), `⌘⇧D` (split down), and `⌘⇧W` (close pane).

## Driving it with kumu-verify.sh

Preconditions:

- `kumu-verify.sh doctor` succeeds with `panes >= 1`.
- `$KUMU_VERIFY_EVIDENCE` exists.

- **List panes.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli panes --json | tee "$KUMU_VERIFY_EVIDENCE/panes/list.json"`. Exit code `0`; stdout is a JSON array with at least one pane object containing `id`, `focused: true`, `exited: false`, `rows > 0`, and `cols > 0`.
- **Read focused pane.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli pane read focused --history 200 --json | tee "$KUMU_VERIFY_EVIDENCE/panes/read.json"`. Exit code `0`; stdout JSON includes `pane` metadata matching the focused pane ID and a `text` field.
- **Refuse shell send.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli pane send focused "echo should-not-run" --json`. Exit code is non-zero (`1`), because Kumu refuses to inject text into a plain shell pane.
- **Split agent pane (requires an installed agent CLI, e.g. `codex` or `claude`).** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli pane split right --agent codex --json | tee "$KUMU_VERIFY_EVIDENCE/panes/split.json"`. Exit code `0` and a new pane ID appears in a follow-up `kumu-verify.sh cli panes --json`.
- **Close split pane.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli pane close <NEW_PANE_ID> --json` and confirm it disappears from `kumu-verify.sh cli panes --json`.

## Gotchas

- `kumu pane send` intentionally rejects plain shell panes to prevent arbitrary command injection; only agent panes accept `send`.
- `kumu pane split --agent <id>` requires the `<id>` CLI binary to be installed on `PATH`; if no agent CLI is installed in the test environment, report the unmet precondition rather than substituting a fake command.
- Relative targets (`left`, `right`, `up`, `down`) require `KUMU_PANE` in the environment; outside a pane, address panes by numeric ID or `focused`.
