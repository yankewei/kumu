# First-run onboarding wizard

First-run onboarding automatically opens a 4-step NSH overlay (`接入 Agent` → `通知提醒` → `项目与窗格` → `体验 Artifacts`) on a brand-new persistent workspace when `onboarding_done` is not `"1"`, and can be reopened later via `⌘K`.

## Sub-features

- `onboarding-auto-open` displays the wizard on first launch with an empty `HOME/.kumu` database (`first_run == true` and `onboarding_done != "1"`).
- `onboarding-steps` steps through all four pages (`Agents`, `Notifications`, `Workspace`, `Artifacts`).
- `onboarding-persist-done` writes `onboarding_done = "1"` into SQLite `settings` upon completion or skip so subsequent launches do not auto-open it.

## How to get to it (user POV)

- Launch Kumu for the first time with an empty `~/.kumu` directory.
- Press `⌘K` in the main window and choose **打开新手引导** to reopen the wizard at any time.

## Driving it with kumu-verify.sh

Preconditions:

- `kumu-verify.sh launch` started Kumu with a brand-new empty `$KUMU_VERIFY_HOME`.
- `kumu-verify.sh doctor` succeeds.

- **Verify first-run DB state.** Run `sqlite3 -readonly "$KUMU_VERIFY_HOME/.kumu/kumu.db" "SELECT key, value FROM settings ORDER BY key;" | tee "$KUMU_VERIFY_EVIDENCE/onboarding/db-initial.txt"`. `seeded|1` is present and `onboarding_done` is absent (not `"1"`).
- **Capture initial onboarding overlay.** Run `screencapture -x "$KUMU_VERIFY_EVIDENCE/onboarding/first-run.png"` while the isolated Kumu window is visible to confirm the 4-step wizard overlay renders on top of the workspace.
- **Verify pure state machine transitions.** Run `cargo test --bin kumu nsh_ui::onboarding:: -- --nocapture 2>&1 | tee "$KUMU_VERIFY_EVIDENCE/onboarding/unit.txt"`. Exit code `0`; step transitions, `should_auto_open_onboarding`, `step_completed`, and `FeedbackMessage` handling pass.

## Gotchas

- `target/debug/kumu` is a bare binary, so Step 2's macOS notification banner test will not display a native macOS banner unless tested in a `.app` bundle built with `packaging/make-app.sh release`.
- GPUI/NSH desktop controls are not exposed to CDP; automated UI driving of onboarding clicks requires macOS Accessibility/UI scripting or visual inspection of the running window, while DB state and `nsh_ui::onboarding` transitions are verified programmatically.
