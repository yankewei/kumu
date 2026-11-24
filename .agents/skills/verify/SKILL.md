---
name: verify
description: "Drive the real Kumu macOS desktop app and its local CLI control channel in an isolated instance to prove user-facing behavior (workspace/panes, TODO board, cross-pane agent collaboration, and agent notification hooks) with durable evidence. Use after making changes to Kumu, before declaring a fix or feature done, or whenever you need to verify behavior against the running app instead of unit tests alone."
---

# Verify Kumu

Kumu is a native macOS desktop application (Rust + Pure Rust NSH + GPUI + embedded Ghostty VT) with a built-in CLI control channel (`kumu panes`, `kumu pane ...`, `kumu todo ...`, `kumu --kumu-notify ...`) served over a per-instance Unix domain socket (`~/.kumu/control.sock`) and backed by local SQLite (`~/.kumu/kumu.db`).

All verification runs use an isolated `HOME` under `/tmp/kumu-verify-<RUN_ID>/` so multiple runs can execute side-by-side without touching the user's real `~/.kumu/kumu.db` or `~/.kumu/control.sock`. Proof artifacts are written under `/tmp/kumu-verify-evidence/<RUN_ID>/` and survive cleanup.

Read [features/README.md](./features/README.md) before driving a feature, then follow the matching feature recipe.

## Launch

Build the debug binary once (dependencies are already compiled at `opt-level = 3` in `Cargo.toml`), then launch an isolated Kumu instance with the helper script:

```sh
cargo build
.agents/skills/verify/scripts/kumu-verify launch --run-id "<RUN_ID>" --project "$(pwd)"
```

What `launch` does:
- Creates a disposable home directory at `/tmp/kumu-verify-<RUN_ID>/home` and an evidence directory at `/tmp/kumu-verify-evidence/<RUN_ID>`.
- Clears inherited `KUMU_*` variables (`KUMU_APP_PID`, `KUMU_PANE`, `KUMU_CONTROL_SOCKET`, `KUMU_APP_BINARY`, `KUMU_TTY`, `KUMU_TERMINAL`, `KUMU_SESSION_TOKEN`) so running inside an existing Kumu pane never leaks into or targets the host instance.
- Starts `target/debug/kumu` with `HOME=/tmp/kumu-verify-<RUN_ID>/home`, `KUMU_CONTROL_SOCKET=/tmp/kumu-verify-<RUN_ID>/home/.kumu/control.sock`, and `KUMU_LOG_TO_FILE=0`, redirecting stderr to `/tmp/kumu-verify-<RUN_ID>/kumu.log`.
- From an empty database, Kumu runs SQLite migrations (`Store::open`), seeds `--project` (the process `cwd`) as the first-run project (`SETTING_SEEDED = "1"`), opens the main window (`[ui] window opened`), and spawns a live shell pane.
- Polls until `control.sock` responds to `panes --json` with at least one running pane (`exited == false`) owned by the launched PID.

Teardown is always explicit:

```sh
.agents/skills/verify/scripts/kumu-verify cleanup "<RUN_ID>"
```

## Doctor

Run this read-only check before driving any feature, and again whenever anything looks off:

```sh
.agents/skills/verify/scripts/kumu-verify doctor "<RUN_ID>"
```

`doctor` verifies and prints:
- The recorded PID is alive and its executable matches `target/debug/kumu` (with build mtime).
- The isolated `HOME`, `kumu.db`, and `control.sock` exist inside `/tmp/kumu-verify-<RUN_ID>/home/.kumu/`.
- `kumu panes --json` succeeds with `KUMU_APP_PID=<PID>` (proving the socket is served by **this** instance, not another Kumu process) and reports at least one non-exited pane.

Never drive an instance that `doctor` reports unhealthy or that was not launched by your verification run.

## Drive

Drive the running instance through the helper's `cli` and `db` subcommands (never target the user's default `~/.kumu/control.sock`):

```sh
# Control CLI commands (automatically bound to RUN_ID's socket & PID)
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- panes --json
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane read focused --history 200 --tail 40 --json
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane scroll up focused --lines 10 --json
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo list --json
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo add "Smoke task" --desc "Verify CLI + DB" --json
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo review <ID> --json
.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo remove <ID> --json

# Read-only second view of SQLite persistence
.agents/skills/verify/scripts/kumu-verify db "<RUN_ID>" "SELECT id, name, path FROM projects;"
.agents/skills/verify/scripts/kumu-verify db "<RUN_ID>" "SELECT id, title, status, awaiting_review FROM todos;"

# Agent hook transport verification (runs detached PTYs against target/debug/kumu)
python3 scripts/test-agent-integration.py
```

For feature-specific journeys, preconditions, and gotchas, use the feature map in [features/README.md](./features/README.md):
- [Workspace and panes](./features/workspace-and-panes.md)
- [TODO board](./features/todo-board.md)
- [Agent pane collaboration](./features/agent-pane-collaboration.md)
- [Agent notifications](./features/agent-notifications.md)

## Evidence

Write all proof artifacts into `/tmp/kumu-verify-evidence/<RUN_ID>/<feature>/`:
- Record the feature ID, entry point, exact command, exit code, stdout/stderr, and resulting state — not just the final screen.
- For mutations (such as TODO creation/updates or workspace seeding), capture a **read-only second view** from SQLite via `kumu-verify db "<RUN_ID>" ...` alongside the CLI response.
- Exercise the real user path over `control.sock` and the running binary; do not substitute unit tests or internal setters for runtime proof.
- For optional visual proof on a desktop session with Screen Recording permission, run `.agents/skills/verify/scripts/kumu-verify screenshot "<RUN_ID>" /tmp/kumu-verify-evidence/<RUN_ID>/<feature>/window.png`. If macOS denies Screen Recording permission in a headless/SSH shell, record the exact error rather than faking a screenshot.

## Cleanup

Always tear down the instance you started — including after failed runs — using:

```sh
.agents/skills/verify/scripts/kumu-verify cleanup "<RUN_ID>"
```

Cleanup rules:
- Sends `SIGTERM` (and `SIGKILL` after 5s if still alive) **only** to the PID recorded in `/tmp/kumu-verify-<RUN_ID>/pid`. Never `pkill kumu` or kill by process name.
- Removes `/tmp/kumu-verify-<RUN_ID>/` (disposable `HOME`, `kumu.db`, `control.sock`, and runtime log).
- **Never** deletes `/tmp/kumu-verify-evidence/<RUN_ID>/`. After cleanup, verify your proof files still exist there.

## Helpers

Executable helper shipped with this skill:
- `.agents/skills/verify/scripts/kumu-verify`
  - `kumu-verify launch [--run-id <id>] [--project <dir>] [--bin <path>]`
  - `kumu-verify doctor <run-id>`
  - `kumu-verify cli <run-id> -- <kumu cli args...>`
  - `kumu-verify db <run-id> "<sql>"`
  - `kumu-verify screenshot <run-id> <output-png>`
  - `kumu-verify cleanup <run-id>`
