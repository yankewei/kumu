# Kumu verification map

This directory is the maintained source for verifying user-facing behavior in Kumu. Read this index before driving the app, then follow the matching feature file as the recipe.

## Baseline preconditions

- Build the debug binary with `cargo build` so `target/debug/kumu` reflects the current checkout.
- Launch an isolated instance with `.agents/skills/verify/scripts/kumu-verify launch --run-id "<RUN_ID>" --project "$(pwd)"`.
- Confirm `.agents/skills/verify/scripts/kumu-verify doctor "<RUN_ID>"` reports healthy process, binary, SQLite DB, Unix socket, and at least one live pane owned by `<RUN_ID>`.
- Never drive an instance that was not started by this verification run, and never target the user's real `~/.kumu/control.sock` or `~/.kumu/kumu.db`.

## Driving conventions

- Start every recipe from the baseline isolated instance unless its preconditions say otherwise.
- Run control commands through `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- <args>` so `HOME`, `KUMU_CONTROL_SOCKET`, and `KUMU_APP_PID` stay pinned to the isolated run.
- Prefer explicit pane targets (`focused`, `right`, `down`, or numeric pane IDs from `panes --json`) and `--json` output over scraping human-formatted tables.
- Verify persistent mutations with a read-only second view via `.agents/skills/verify/scripts/kumu-verify db "<RUN_ID>" "<sql>"`.
- Always run `.agents/skills/verify/scripts/kumu-verify cleanup "<RUN_ID>"` after finishing (including on failure). Cleanup removes `/tmp/kumu-verify-<RUN_ID>/` and preserves `/tmp/kumu-verify-evidence/<RUN_ID>/`.

## Proof and skip reporting

- Capture both the command/action and the resulting observable state, not only the final state.
- CLI proof includes the exact command, exit code, stdout, and stderr saved under `/tmp/kumu-verify-evidence/<RUN_ID>/<feature>/`.
- Mutation proof includes a read-only SQLite query from `kumu-verify db` confirming the persisted row(s).
- Record the feature ID and entry point used with every artifact.
- If a feature precondition is unmet (for example, no external coding agent binary is installed on the machine for `pane split --agent`), report the exact attempted command and unmet precondition; never weaken the recipe to make it pass.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behavior, followed by exactly four H2 sections in this order:

1. `Sub-features` lists short IDs with one line for each behavior.
2. `How to get to it (user POV)` lists every user entry point.
3. `Driving it with <harness>` starts with `Preconditions:` and uses labeled bullets pairing each user action with an exact command and observable result.
4. `Gotchas` lists traps that can waste or invalidate a verification run.

## Features

- [Workspace and panes](./workspace-and-panes.md) covers first-run project seeding, pane listing, terminal reading/scrolling, shell-pane send protection, and SQLite workspace state.
- [TODO board](./todo-board.md) covers adding, listing, inspecting, updating, reviewing, completing, and deleting TODO items with SQLite persistence.
- [Agent pane collaboration](./agent-pane-collaboration.md) covers splitting an agent pane, sending follow-up prompts, waiting for completion/input, and closing temporary panes.
- [Agent notifications](./agent-notifications.md) covers `--kumu-notify` hook transport for Claude, Codex, Grok, Cursor, and Antigravity CLI across detached PTYs.
