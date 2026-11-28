---
name: verify-kumu
description: "Drive a real isolated Kumu macOS desktop instance and its control CLI (panes, artifacts, agent integrations, SQLite state) to verify user-facing behavior and capture surviving proof artifacts. Use when proving UI, pane control, artifact publishing, onboarding, or integration changes work end to end."
---

# Verify Kumu

Kumu is a native macOS desktop app (Rust + GPUI + NSH + libghostty) with a built-in local Unix-socket control CLI (`kumu panes`, `kumu pane ...`, `kumu artifact ...`, `kumu integration ...`, `kumu mcp ...`) and SQLite persistence at `$HOME/.kumu/kumu.db`.

Read [`features/README.md`](features/README.md) before driving the app, then follow the matching feature recipe.

## Isolation

Two Kumu instances can run side by side as long as each instance gets its own `HOME` and `KUMU_CONTROL_SOCKET`:

- `db_path()` reads `$HOME/.kumu/kumu.db`.
- `socket_path()` reads `$KUMU_CONTROL_SOCKET` (defaulting to `$HOME/.kumu/control.sock`).
- When verification runs from inside a Kumu terminal pane, `kumu-verify.sh` unsets `KUMU_APP_PID`, `KUMU_PANE`, `KUMU_TERMINAL`, `KUMU_TTY`, and `KUMU_SESSION_TOKEN` so CLI calls target the isolated instance rather than rejecting its PID as belonging to another Kumu.

Never drive the user's live `~/.kumu/control.sock` or `~/.kumu/kumu.db`. Always use the helper script below, which creates a disposable `HOME` under `/tmp/kumu-verify-<run-id>` and writes evidence to a separate directory that survives cleanup.

## Launch

Use the helper script [`scripts/kumu-verify.sh`](scripts/kumu-verify.sh):

```sh
# 0. In a fresh git worktree, initialize the ghostty submodule once if vendor/ghostty/build.zig is missing
[ -f vendor/ghostty/build.zig ] || git submodule update --init --depth 1 vendor/ghostty

# 1. Build debug binary if missing or stale
cargo build

# 2. Launch an isolated Kumu instance from an empty data dir
eval "$(.agents/skills/verify-kumu/scripts/kumu-verify.sh launch)"
```

`launch` does:
1. Creates `/tmp/kumu-verify-$RUN_ID/home` (empty `HOME`, so SQLite migrates from scratch and seeds the first-run project) and `/tmp/kumu-verify-$RUN_ID/project` (a disposable git repo for artifact/pane tests).
2. Creates an evidence directory at `/tmp/kumu-verify-evidence-$RUN_ID` (outside the disposable instance dir so `cleanup` never deletes proof artifacts).
3. Starts `./target/debug/kumu` with `HOME=$KUMU_VERIFY_HOME` and `KUMU_CONTROL_SOCKET=$KUMU_VERIFY_SOCKET`, recording its PID in `$KUMU_VERIFY_ROOT/kumu.pid`, stdout in `$KUMU_VERIFY_ROOT/kumu.stdout.log`, and stderr in `$KUMU_VERIFY_ROOT/kumu.stderr.log`.
4. Polls until `$KUMU_VERIFY_SOCKET` accepts a `kumu panes --json` request returning a non-empty JSON array (usable state, not just process alive).
5. Prints shell `export` lines for `KUMU_VERIFY_RUN_ID`, `KUMU_VERIFY_ROOT`, `KUMU_VERIFY_HOME`, `KUMU_VERIFY_SOCKET`, `KUMU_VERIFY_PROJECT`, `KUMU_VERIFY_EVIDENCE`, `KUMU_VERIFY_PID`, and `KUMU_VERIFY_BIN`.

> Note on macOS system notifications: `cargo build` produces a bare binary (`target/debug/kumu`), not a `.app` bundle. macOS notification banners require `packaging/make-app.sh release`; control socket, panes, SQLite, artifacts, onboarding UI, and hook transport all work on `target/debug/kumu`.

## Doctor

Run this read-only check first before driving or whenever anything looks off:

```sh
.agents/skills/verify-kumu/scripts/kumu-verify.sh doctor
```

`doctor` verifies:
- `$KUMU_VERIFY_PID` is alive and its binary matches `$KUMU_VERIFY_BIN`.
- `$KUMU_VERIFY_SOCKET` exists as a socket and responds to `kumu panes --json` with valid JSON.
- `$KUMU_VERIFY_HOME/.kumu/kumu.db` exists and has `PRAGMA user_version` > 0.
- Refuses to proceed if `KUMU_VERIFY_SOCKET` points at the user's real `~/.kumu/control.sock`.

## Drive

Always invoke the built binary with `HOME="$KUMU_VERIFY_HOME"` and `KUMU_CONTROL_SOCKET="$KUMU_VERIFY_SOCKET"` (or via the `kumu-verify.sh cli` wrapper, which sets both automatically):

```sh
# List panes as JSON
.agents/skills/verify-kumu/scripts/kumu-verify.sh cli panes --json

# Read the focused pane's terminal screen + scrollback
.agents/skills/verify-kumu/scripts/kumu-verify.sh cli pane read focused --history 200 --json

# Publish / list / inspect / open / remove an artifact in the disposable project
.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact publish "$FILE" --title "Verify" --project "$KUMU_VERIFY_PROJECT" --json
.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact list --project "$KUMU_VERIFY_PROJECT" --json

# Inspect integration status (isolated HOME)
.agents/skills/verify-kumu/scripts/kumu-verify.sh cli integration status --json

# Verify hook transport (OSC 777 sequences over real PTYs, no live GUI needed)
python3 scripts/test-agent-integration.py
```

For visual UI proof (onboarding overlay, pane layout, Artifacts drawer), capture the window or screen with macOS `screencapture -x "$KUMU_VERIFY_EVIDENCE/<name>.png"` while the isolated Kumu window is visible, and inspect persisted DB state read-only with:

```sh
sqlite3 -readonly "$KUMU_VERIFY_HOME/.kumu/kumu.db" "SELECT key, value FROM settings ORDER BY key;"
```

## Evidence

Store all proof artifacts under `$KUMU_VERIFY_EVIDENCE` (default `/tmp/kumu-verify-evidence-$RUN_ID`):

- **Exercise the real user path**: drive through `kumu` CLI / control socket / UI, never by mutating SQLite rows directly to simulate an action.
- **Capture action + resulting state**: save both the command/JSON output of the action and a second read-only view (e.g. `kumu panes --json`, `kumu artifact list --json`, `sqlite3 -readonly`, or `screencapture` PNG).
- **Verify side effects**: check files on disk (e.g. `$KUMU_VERIFY_PROJECT/.kumu/artifacts/<id>/`) and SQLite rows alongside CLI/UI output.
- **No silent skips**: if a journey requires a precondition (e.g. an installed agent CLI like `codex` for `pane split --agent`), report the unmet precondition instead of weakening the check.

## Cleanup

Tear down only the PID and scratch directory created by this run:

```sh
.agents/skills/verify-kumu/scripts/kumu-verify.sh cleanup
```

`cleanup`:
- Sends `SIGTERM` (then `SIGKILL` after 5s if still alive) **only** to the PID recorded in `$KUMU_VERIFY_ROOT/kumu.pid` — never `killall` or `pkill` by process name.
- Removes `$KUMU_VERIFY_ROOT` (`/tmp/kumu-verify-$RUN_ID`).
- **Never** removes `$KUMU_VERIFY_EVIDENCE` (`/tmp/kumu-verify-evidence-$RUN_ID`); proof artifacts survive teardown.

## Helpers

- [`scripts/kumu-verify.sh`](scripts/kumu-verify.sh) (`chmod +x`):
  - `launch` — start isolated Kumu instance and print `export` statements
  - `doctor` — read-only health check of PID, socket, panes, and SQLite DB
  - `cli <args...>` — run `$KUMU_VERIFY_BIN` against the isolated `HOME` and `KUMU_CONTROL_SOCKET`
  - `cleanup` — terminate only this run's PID and delete `$KUMU_VERIFY_ROOT`, preserving `$KUMU_VERIFY_EVIDENCE`
