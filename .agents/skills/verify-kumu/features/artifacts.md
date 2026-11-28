# Artifacts publishing & management

Artifacts lets an agent or user publish Markdown, HTML, or text documents into `<project>/.kumu/artifacts/<id>/`, list and inspect them, replace them in place, notify a running Kumu instance to refresh its Artifacts drawer, and remove them.

## Sub-features

- `artifact-publish` copies a source document into `<project>/.kumu/artifacts/<id>/` with metadata and notifies the running Kumu instance.
- `artifact-list` lists published artifacts for a project in JSON or human-readable form.
- `artifact-path` resolves the stored on-disk entry path for an artifact ID.
- `artifact-replace` updates an existing artifact in place while preserving its ID.
- `artifact-remove` deletes an artifact when `--force` is passed non-interactively.

## How to get to it (user POV)

- Run `kumu artifact publish|list|path|open|remove ...` in a terminal or via the built-in MCP server.
- Open the right-hand **Artifacts** drawer in the Kumu window to browse and read published documents.

## Driving it with kumu-verify.sh

Preconditions:

- `kumu-verify.sh doctor` succeeds.
- `$KUMU_VERIFY_PROJECT` is a disposable git repository created by `launch`.

- **Create sample document.** Write a Markdown file: `printf '# Verify Plan\n\nInitial body.\n' > "$KUMU_VERIFY_ROOT/plan.md"`.
- **Publish artifact.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact publish "$KUMU_VERIFY_ROOT/plan.md" --title "Verify Plan" --project "$KUMU_VERIFY_PROJECT" --json | tee "$KUMU_VERIFY_EVIDENCE/artifacts/publish.json"`. Exit code `0`; stdout JSON includes `id`, `title: "Verify Plan"`, and `kind: "markdown"`.
- **Confirm via list (second view).** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact list --project "$KUMU_VERIFY_PROJECT" --json | tee "$KUMU_VERIFY_EVIDENCE/artifacts/list.json"`. Exit code `0`; the returned array contains the published `id` and title `"Verify Plan"`.
- **Confirm on-disk side effect.** Extract `ID` from `publish.json`, run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact path "$ID" --project "$KUMU_VERIFY_PROJECT"`, and verify the returned file exists and contains `Initial body.`.
- **Replace in place.** Update `$KUMU_VERIFY_ROOT/plan.md` with `Updated body.` and run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact publish "$KUMU_VERIFY_ROOT/plan.md" --title "Verify Plan v2" --replace "$ID" --project "$KUMU_VERIFY_PROJECT" --json | tee "$KUMU_VERIFY_EVIDENCE/artifacts/replace.json"`. Confirm `id` is unchanged and on-disk content contains `Updated body.`.
- **Remove artifact.** Run `.agents/skills/verify-kumu/scripts/kumu-verify.sh cli artifact remove "$ID" --project "$KUMU_VERIFY_PROJECT" --force` and confirm `kumu-verify.sh cli artifact list --project "$KUMU_VERIFY_PROJECT" --json` no longer lists `$ID`.

## Gotchas

- `artifact remove` requires `--force` when stdin is not a TTY; omitting it fails with a non-zero exit code.
- Always pass `--project "$KUMU_VERIFY_PROJECT"` when invoking from outside a Kumu pane so artifacts are written to the disposable repo, never the real Kumu source checkout.
- Passing `--open` opens the reading sheet in the running Kumu window; pair it with `screencapture -x` if verifying visual rendering.
