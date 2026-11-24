# TODO board

Kumu's right-hand TODO board is scoped per root Git repository (worktrees share their parent repository's board) and can be managed from both the UI panel and the `kumu todo` CLI, persisting every transition to SQLite.

## Sub-features

- `todo-add` creates a task with title, description, and initial status.
- `todo-list-show` lists repository tasks and inspects a single task's full details.
- `todo-update-review` updates fields and transitions a task to `in_progress` with `awaitingReview: true` via `todo review`.
- `todo-done-remove` marks a task `done` (recording `completedAt`) and deletes a task via `todo remove`.

## How to get to it (user POV)

- Open the right-hand TODO plugin panel in the Kumu window, type in the quick-add input (`Enter` to add, `⌘Enter` to add and dispatch), drag cards across lanes, or use card actions.
- Run `"$KUMU_APP_BINARY" todo list|show|add|update|review|done|remove` from a Kumu pane in the repository.

## Driving it with kumu-verify

Preconditions:

- An isolated Kumu instance `<RUN_ID>` is running and `kumu-verify doctor "<RUN_ID>"` passes.
- The evidence directory `/tmp/kumu-verify-evidence/<RUN_ID>/todo-board` exists.

- **Check empty initial list.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo list --json`. Exit code `0`; returns `[]` on a fresh isolated DB.
- **Add a task.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo add "Verify persistence" --desc "End-to-end check" --json`. Exit code `0`; JSON contains numeric `id`, `"title": "Verify persistence"`, `"description": "End-to-end check"`, `"status": "todo"`, `"awaitingReview": false`.
- **Inspect task and list.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo show <ID> --json` and `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo list --json`. Both return exit code `0` and include the created task.
- **Move to in-progress and request review.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo update <ID> --status in_progress --json` followed by `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo review <ID> --json`. Exit code `0`; JSON shows `"status": "in_progress"` and `"awaitingReview": true`.
- **Verify SQLite persistence (second view).** Run `.agents/skills/verify/scripts/kumu-verify db "<RUN_ID>" "SELECT id, title, status, awaiting_review, completed_at FROM todos WHERE id = <ID>;"`. Row matches `Verify persistence`, `in_progress`, `awaiting_review = 1`, `completed_at` empty.
- **Mark done and verify.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo done <ID> --json` and re-query `kumu-verify db`. JSON shows `"status": "done"`, `"awaitingReview": false`, and non-empty `"completedAt"`; SQLite row matches.
- **Remove task and confirm deletion.** Run `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo remove <ID> --json` and `.agents/skills/verify/scripts/kumu-verify cli "<RUN_ID>" -- todo list --json`. List returns `[]`, and SQLite `SELECT COUNT(*) FROM todos;` returns `0`.
- **Proof.** Save the CLI JSON responses and SQLite query outputs into `/tmp/kumu-verify-evidence/<RUN_ID>/todo-board/`.

## Gotchas

- `todo add --bind` and `todo update --bind` require `$KUMU_PANE` to be an agent pane (or a live pane in the target project); from outside a pane, omit `--bind` unless you explicitly pass `KUMU_PANE=<pane_id>`.
- `todo review <ID>` keeps the task in `in_progress` and sets `awaitingReview: true`; it does **not** move the task to `done`.
- Always assert the SQLite row via `kumu-verify db` in addition to CLI stdout, so in-memory-only bugs are caught.
