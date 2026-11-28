# Kumu verification map

This directory is the maintained source for verifying user-facing behavior of Kumu. Read this index before driving the app, then follow the matching feature recipe.

## Baseline preconditions

- Build the debug binary with `cargo build` (`./target/debug/kumu`).
- Launch an isolated instance with `eval "$(.agents/skills/verify-kumu/scripts/kumu-verify.sh launch)"`.
- Confirm `KUMU_VERIFY_HOME` and `KUMU_VERIFY_SOCKET` point inside `/tmp/kumu-verify-<run-id>`, never `~/.kumu`.
- Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh doctor` and require `doctor ok` with `panes >= 1` and `schema_v > 0`.
- Never drive an instance that was not started by this verification run.

## Driving conventions

- Start every recipe from the baseline isolated state unless its preconditions say otherwise.
- Run control CLI commands through `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli <args...>` so `HOME` and `KUMU_CONTROL_SOCKET` always stay bound to the isolated instance.
- Treat commands and flags as literal.
- Preserve proof artifacts under `$KUMU_VERIFY_EVIDENCE` (never inside `$KUMU_VERIFY_ROOT`).

## Proof and skip reporting

- Capture both the user action and the resulting state, not only the final screen or exit code.
- CLI / control proof includes the exact command, stdout JSON/text, stderr, and exit code saved under `$KUMU_VERIFY_EVIDENCE/<feature>/`.
- Mutation proof includes a second read-only view (e.g. `kumu artifact list --json`, `kumu panes --json`, on-disk files, or `sqlite3 -readonly "$KUMU_VERIFY_HOME/.kumu/kumu.db"`).
- UI visual proof captures a macOS screenshot with `screencapture -x "$KUMU_VERIFY_EVIDENCE/<feature>/<name>.png"` while the isolated Kumu window is visible.
- Report any unreachable path with the attempted command and unmet precondition; never claim a skipped entry point passed via another path.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behavior, followed by four H2 sections in order:

1. `Sub-features` — short IDs with one line per behavior.
2. `How to get to it (user POV)` — every user entry point.
3. `Driving it with kumu-verify.sh` — starts with `Preconditions:` and pairs each action with an exact command and observable result.
4. `Gotchas` — traps that can waste or invalidate a verification run.

## Features

- [Pane control & inspection](./panes.md) covers listing panes, reading terminal output, splitting agent panes, and closing panes over the control socket.
- [Artifacts publishing & management](./artifacts.md) covers publishing, listing, replacing, inspecting path, opening, and removing Markdown/HTML artifacts.
- [Agent integration & hook transport](./integration.md) covers checking integration status in an isolated `HOME` and verifying OSC 777 hook delivery over real PTYs.
- [First-run onboarding wizard](./onboarding.md) covers automatic first-run launch, 4-step wizard UI visibility, and `onboarding_done` persistence in SQLite.
