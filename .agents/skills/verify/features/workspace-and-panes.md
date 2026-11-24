# Workspace and panes

Opening Kumu in a fresh workspace seeds the current directory as a project, opens a live terminal pane, lets scripts or agents inspect and scroll pane content over the local control socket, and protects plain shell panes from accidental `pane send` input.

## Sub-features

- `pane-bootstrap` seeds the starting directory as a project and opens a live focused shell pane from an empty database.
- `pane-list` lists active panes with id, project, title, cwd, dimensions, focus, and exit state.
- `pane-read` reads live terminal text and optional scrollback history from a pane.
- `pane-scroll` scrolls a pane's viewport up or down without mutating the shell process.
- `pane-send-guard` rejects `pane send` against a non-agent shell pane with `notAgent`.

## How to get to it (user POV)

- Open Kumu from Finder/Dock or `cargo run` to enter the main workspace with a seeded project and terminal pane.
- Run `"$KUMU_APP_BINARY" panes`, `pane read`, `pane scroll`, or `pane send` from inside any Kumu pane.

## Driving it with kumu-verify

Preconditions:

- An isolated Kumu instance `<RUN_ID>` is running against the repository directory and `kumu-verify doctor "<RUN_ID>"` passes.
- The evidence directory `/tmp/kumu-verify-evidence/<RUN_ID>/workspace-and-panes` exists (`mkdir -p`).

- **List panes.** Inspect the bootstrapped workspace. Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- panes --json`. Exit code is `0`; JSON array has at least one pane with `"focused": true`, `"exited": false`, `"agent": null`, and `"cwd"` matching the launched `--project` directory.
- **Read focused pane.** Read screen and scrollback text. Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane read focused --history 200 --tail 40 --json`. Exit code is `0`; JSON includes `"id"`, `"cols"`, `"rows"`, and `"text"` containing the shell prompt or terminal output.
- **Scroll pane viewport.** Scroll up and back down. Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane scroll up focused --lines 5 --json` and `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane scroll down focused --lines 5 --json`. Both return exit code `0` with JSON scroll results.
- **Verify shell send guard.** Attempt to send text to the shell pane. Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- pane send focused "echo should-not-run" --json`. Command fails with non-zero exit code and stderr mentioning `notAgent` (confirming shell panes cannot be injected with arbitrary commands).
- **Confirm SQLite bootstrap state.** Query the isolated database read-only. Run `.agents/skills/verify/scripts/kumu-verify db "<RUN_ID>" "SELECT id, name, path FROM projects; SELECT key, value FROM settings WHERE key = 'seeded';"`. Output shows the seeded project row matching `--project` and `seeded = 1`.
- **Proof.** Save the `panes --json`, `pane read`, `pane send` error, and `db` outputs into `/tmp/kumu-verify-evidence/<RUN_ID>/workspace-and-panes/`.

## Gotchas

- Running `kumu` directly inside an existing Kumu terminal without unsetting `KUMU_APP_PID` and `KUMU_CONTROL_SOCKET` talks to the outer Kumu window or fails with a PID mismatch. Always go through `kumu-verify cli`.
- On a freshly spawned shell, `pane read` may briefly return empty text before the login shell draws its prompt; wait 0.5–1s if testing prompt text.
- `pane scroll` on a pane with no scrollback history still succeeds cleanly; it does not error just because the viewport is already at the top/bottom.
