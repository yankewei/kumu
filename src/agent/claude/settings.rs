//! Claude Code's own configuration files, read and never written.
//!
//! Two of the session's axes cannot be discovered over the wire: the CLI
//! reports no effort in force anywhere, and the mode it reports is the one in
//! force rather than the one a fresh session would start in. Both are in its
//! configuration files, so this is where the settings window's best guess comes
//! from — and where a live session explains an axis it cannot be told about. A
//! session's own report corrects the mode once it connects; see
//! [`effort_in_force`] for why the effort takes a second look at that point.
//!
//! The effort keys, as Claude Code resolves them (see
//! <https://code.claude.com/docs/en/settings-reference#modelsettings>):
//!
//!   - `CLAUDE_CODE_EFFORT_LEVEL`, from the process environment or a settings
//!     `env` block — an explicit choice, and the only source that also accepts
//!     the session-only `max`;
//!   - `modelSettings.<model>.effortLevel` — the per-model level `/effort` and
//!     the `/model` picker's slider save, keyed by the model's canonical name;
//!   - `effortLevel` — the level for models that have none of their own.
//!
//! Across files the CLI resolves each model separately: the highest-precedence
//! file that sets either that model's entry or the top-level key decides, which
//! is why the lookups below walk the files in order rather than merging them.
//! Two things are deliberately not modelled: a per-model `maxEffortLevel` cap,
//! and the hold the CLI keeps on Fable 5, Opus 4.8 and Opus 4.7's default effort
//! across sessions. Both live inside the CLI, and a guess would put a number on
//! the axis that nothing here can verify.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The mode `-p` starts in when nothing configures one: Manual, which Claude
/// Code reports as `default` and kumu labels 监督.
const DEFAULT_PERMISSION_MODE: &str = "default";

/// The four levels a settings file accepts, lowest first.
///
/// `max` is session-only and `ultracode` is a workflow setting rather than a
/// level, so neither may appear in a file — the CLI reports a settings error for
/// one, and kumu must not claim a level nothing will run at.
const LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// The files a fresh session's settings come from, most authoritative first.
pub(super) fn settings_paths() -> Vec<PathBuf> {
    vec![managed_settings_path(), config_dir().join("settings.json")]
}

/// Where this build keeps managed settings. The directories are Claude Code's
/// own, quoted from its 2.1.269 binary.
fn managed_settings_path() -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode"
    } else if cfg!(target_os = "windows") {
        r"C:\Program Files\ClaudeCode"
    } else {
        "/etc/claude-code"
    };
    PathBuf::from(dir).join("managed-settings.json")
}

/// Claude Code's configuration directory, resolved the way the CLI itself does.
fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .unwrap_or_default()
}

fn read_settings(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The `permissions.defaultMode` a fresh session would start in, from the first
/// file that names one.
pub(super) fn permission_mode() -> String {
    permission_mode_from(&settings_paths())
}

/// The `permissions.defaultMode` these files resolve to.
fn permission_mode_from<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> String {
    paths
        .into_iter()
        .find_map(|path| read_settings(path).and_then(|settings| default_mode(&settings)))
        .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_string())
}

fn default_mode(settings: &Value) -> Option<String> {
    let value = settings.get("permissions")?.get("defaultMode")?.as_str()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The effort level in force for `model`, from this machine's configuration.
///
/// `model` is the model a session resolved to, when one is known: a saved level
/// is keyed by model, so a session that reports its own model gets the closer
/// answer. A probe, which starts no session, asks with `None` and hears only
/// what applies to every model.
pub(super) fn effort_in_force(model: Option<&str>) -> Option<String> {
    env_effort().or_else(|| level_from_paths(&settings_paths(), model))
}

/// The effort `CLAUDE_CODE_EFFORT_LEVEL` pins.
///
/// Claude Code reports no effort in force — not on the handshake, not on
/// `system/init` — but this variable overrides whatever a session asks for (the
/// CLI says so itself), so it is the one thing that is knowable before a turn.
/// A GUI launch inherits no shell environment, which is why the settings `env`
/// block is consulted too. Neither is validated here: the variable also takes
/// `max` and `auto`, which no settings file may.
fn env_effort() -> Option<String> {
    non_empty(std::env::var("CLAUDE_CODE_EFFORT_LEVEL").ok())
        .or_else(|| env_effort_from(&settings_paths()))
}

/// The `env` block's `CLAUDE_CODE_EFFORT_LEVEL`, from the first file that sets
/// one.
fn env_effort_from<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Option<String> {
    paths.into_iter().find_map(|path| {
        let settings = read_settings(path)?;
        non_empty(
            settings
                .get("env")?
                .get("CLAUDE_CODE_EFFORT_LEVEL")?
                .as_str()
                .map(str::to_owned),
        )
    })
}

/// The level these files resolve to for `model`: the first file that names
/// either that model's own level or the level for models without one.
fn level_from_paths<'a>(
    paths: impl IntoIterator<Item = &'a PathBuf>,
    model: Option<&str>,
) -> Option<String> {
    paths.into_iter().find_map(|path| {
        let settings = read_settings(path)?;
        model
            .and_then(|model| model_level(&settings, model))
            .or_else(|| level(&settings, "effortLevel"))
    })
}

/// The level saved for `model` in a `modelSettings` entry, which maps a model
/// name to an object carrying its `effortLevel`.
fn model_level(settings: &Value, model: &str) -> Option<String> {
    let entries = settings.get("modelSettings")?.as_object()?;
    entries
        .iter()
        .find(|(name, _)| same_model(name, model))
        .and_then(|(_, entry)| level(entry, "effortLevel"))
}

/// A settable level named by `key`, if the file names one.
fn level(settings: &Value, key: &str) -> Option<String> {
    let value = settings.get(key)?.as_str()?.trim();
    LEVELS.contains(&value).then(|| value.to_string())
}

/// Whether a `modelSettings` key names `model`.
///
/// The CLI writes the model's canonical name and matches its alias, its
/// date-suffixed and `[1m]` spellings, and its provider-specific IDs to that one
/// entry. kumu cannot know the aliases, so it compares the names without the
/// qualifiers a saved entry may omit, which covers the ids a session reports.
fn same_model(name: &str, model: &str) -> bool {
    /// A model name without the `[1m]` and `@date` qualifiers.
    fn base(name: &str) -> &str {
        let name = name.trim();
        let name = name.split('[').next().unwrap_or(name);
        name.split('@').next().unwrap_or(name)
    }
    !base(name).is_empty() && base(name) == base(model)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kumu-claude-settings-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// The mode comes from the first file that names one — managed settings
    /// before the user's — and a file that names none changes nothing.
    #[test]
    fn the_starting_mode_comes_from_the_most_authoritative_file() {
        let dir = settings_dir("mode");
        let managed = write(
            &dir,
            "managed.json",
            r#"{"permissions": {"defaultMode": "acceptEdits"}}"#,
        );
        let user = write(
            &dir,
            "user.json",
            r#"{"permissions": {"defaultMode": "plan"}}"#,
        );
        let absent = dir.join("absent.json");

        assert_eq!(
            permission_mode_from([&managed, &user]),
            "acceptEdits",
            "managed settings win"
        );
        assert_eq!(
            permission_mode_from([&absent, &user]),
            "plan",
            "a missing file is skipped"
        );
        assert_eq!(
            permission_mode_from([&absent]),
            DEFAULT_PERMISSION_MODE,
            "which leaves the documented mode"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Only a non-empty string counts, and a settings `env` block can carry the
    /// effort when the app inherited no shell environment.
    #[test]
    fn settings_are_read_defensively() {
        assert_eq!(
            default_mode(&serde_json::json!({"permissions": {"defaultMode": "  "}})),
            None
        );
        assert_eq!(default_mode(&serde_json::json!({"permissions": {}})), None);
        assert_eq!(default_mode(&serde_json::json!({})), None);

        let dir = settings_dir("env");
        let user = write(
            &dir,
            "user.json",
            r#"{"env": {"CLAUDE_CODE_EFFORT_LEVEL": "high", "OTHER": "x"}}"#,
        );
        // The process environment outranks every file when it sets one, which is
        // what the CLI itself honours.
        assert_eq!(
            env_effort_from([&user]).as_deref(),
            std::env::var("CLAUDE_CODE_EFFORT_LEVEL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .as_deref()
                .or(Some("high"))
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// The level saved for a model beats the level that file sets for every
    /// model, and either one answers for a model that is not the one saved.
    #[test]
    fn a_saved_model_level_beats_the_models_default() {
        let dir = settings_dir("level");
        let file = write(
            &dir,
            "settings.json",
            r#"{"effortLevel": "medium",
                "modelSettings": {"claude-opus-5": {"effortLevel": "xhigh"}}}"#,
        );

        assert_eq!(
            level_from_paths([&file], Some("claude-opus-5")).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            level_from_paths([&file], Some("claude-sonnet-5")).as_deref(),
            Some("medium"),
            "a model with no entry of its own falls back to the shared level"
        );
        assert_eq!(
            level_from_paths([&file], None).as_deref(),
            Some("medium"),
            "a probe knows no model, so only the shared level answers"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// The first file that names a level decides, because the CLI resolves each
    /// model against the files in order rather than merging them — and a value a
    /// file cannot carry leaves the key unset rather than winning.
    #[test]
    fn the_most_authoritative_file_decides() {
        let dir = settings_dir("precedence");
        let managed = write(
            &dir,
            "managed.json",
            r#"{"modelSettings": {"claude-opus-5": {"effortLevel": "low"}}}"#,
        );
        let user = write(
            &dir,
            "user.json",
            r#"{"modelSettings": {"claude-opus-5": {"effortLevel": "xhigh"}}}"#,
        );
        let absent = dir.join("absent.json");

        assert_eq!(
            level_from_paths([&managed, &user], Some("claude-opus-5")).as_deref(),
            Some("low"),
            "managed settings win"
        );
        assert_eq!(
            level_from_paths([&absent, &user], Some("claude-opus-5")).as_deref(),
            Some("xhigh"),
            "a missing file is skipped"
        );

        // `max` is session-only, so the entry carrying it names no level and the
        // shared key behind it answers instead.
        let unsettable = write(
            &dir,
            "unsettable.json",
            r#"{"effortLevel": "high",
                "modelSettings": {"claude-opus-5": {"effortLevel": "max"}}}"#,
        );
        assert_eq!(
            level_from_paths([&unsettable], Some("claude-opus-5")).as_deref(),
            Some("high")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A session reports the model it resolved to, which may carry a `[1m]` or a
    /// provider date suffix that the saved entry does not.
    #[test]
    fn a_saved_level_matches_the_ids_a_session_reports() {
        assert!(same_model("claude-opus-5", "claude-opus-5[1m]"));
        assert!(same_model("claude-opus-5", "claude-opus-5@20250929"));
        assert!(same_model(" claude-opus-5 ", "claude-opus-5"));
        assert!(!same_model("claude-opus-5", "claude-opus-4-8"));
        assert!(!same_model("", ""));
    }

    /// The four settable levels are the only ones a file may claim, and the
    /// whitespace a hand-edited file might carry does not hide one.
    #[test]
    fn only_the_settable_levels_count() {
        let dir = settings_dir("invalid");
        let ultracode = write(&dir, "ultracode.json", r#"{"effortLevel": "ultracode"}"#);
        assert_eq!(level_from_paths([&ultracode], None), None);

        let padded = write(&dir, "padded.json", r#"{"effortLevel": "  high  "}"#);
        assert_eq!(level_from_paths([&padded], None).as_deref(), Some("high"));
        std::fs::remove_dir_all(dir).ok();
    }
}
