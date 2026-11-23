//! JSONC CST hook manipulation, status checking, and atomic hook file updates.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstArray, CstInputValue, CstObject, CstRootNode};
use serde_json::{Value, json};

use super::Status;

pub(super) const MARKER: &str = " # kumu-notifications";
const AGY_HOOK_NAME: &str = "kumu-notifications";

pub(super) fn hook_groups(agent: &str) -> Vec<(&'static str, Value)> {
    let events: Vec<(&'static str, Option<&'static str>, &'static str)> = match agent {
        // PermissionRequest already fires the moment a prompt appears;
        // Notification's permission_prompt/idle_prompt would later repeat the
        // same turn. Keep only MCP elicitation forms, which have no
        // PermissionRequest event of their own.
        "claude" => vec![
            ("SessionStart", None, "idle"),
            ("SessionEnd", None, "exited"),
            ("UserPromptSubmit", None, "start"),
            ("PostToolUse", None, "resume"),
            ("PostToolUseFailure", None, "resume"),
            ("Stop", None, "complete"),
            ("PermissionRequest", None, "permission"),
            ("PreToolUse", Some("AskUserQuestion|ExitPlanMode"), "input"),
            ("Notification", Some("elicitation_dialog"), "input"),
        ],
        // Grok Build has no PermissionRequest; its Notification event is the
        // channel that carries a permission prompt or a question.
        "grok" => vec![("Stop", None, "complete"), ("Notification", None, "input")],
        _ => vec![
            ("UserPromptSubmit", None, "start"),
            ("PostToolUse", None, "resume"),
            ("Stop", None, "complete"),
            ("PermissionRequest", None, "permission"),
            ("PreToolUse", Some("^request_user_input$"), "input"),
        ],
    };
    events
        .into_iter()
        .map(|(event, matcher, kind)| {
            let command = notify_command(agent, kind);
            let mut group =
                json!({"hooks": [{"type": "command", "command": command, "timeout": 3}]});
            if let Some(matcher) = matcher {
                group["matcher"] = json!(matcher);
            }
            (event, group)
        })
        .collect()
}

/// The hook command every agent runs. The environment guards keep a registered
/// hook inert outside a Kumu pane, including in an IDE that reads the same file.
fn notify_command(agent: &str, kind: &str) -> String {
    format!(
        "if [ \"${{KUMU_TERMINAL:-}}\" = 1 ] && [ -n \"${{KUMU_APP_BINARY:-}}\" ]; then \"$KUMU_APP_BINARY\" --kumu-notify {agent} {kind}; fi{MARKER}"
    )
}

/// Preserve JSONC comments, user hooks, and unchanged hook hashes during repair.
pub(super) fn edit_hooks(text: &str, agent: &str, remove: bool) -> io::Result<String> {
    if agent == "cursor" {
        return edit_cursor_hooks(text, remove);
    }
    if agent == "agy" {
        return edit_agy_hooks(text, remove);
    }
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(io::Error::other)?;
    let object = root
        .object_value()
        .ok_or_else(|| io::Error::other("配置必须是 JSON 对象，未覆盖原文件"))?;
    if remove {
        let Some(hooks) = object.object_value("hooks") else {
            return Ok(text.into());
        };
        if !remove_managed_hooks(&hooks) {
            return Ok(text.into());
        }
        if hooks.properties().is_empty()
            && let Some(property) = object.get("hooks")
        {
            property.remove();
        }
        return Ok(root.to_string());
    }
    let hooks = object
        .object_value_or_create("hooks")
        .ok_or_else(|| io::Error::other("hooks 必须是对象，未覆盖原文件"))?;
    let install = hook_groups(agent);
    let known: Vec<&str> = install.iter().map(|(event, _)| *event).collect();
    for (event, expected) in install {
        let groups = hooks
            .array_value_or_create(event)
            .ok_or_else(|| io::Error::other(format!("hooks.{event} 必须是数组，未覆盖原文件")))?;
        let existing = groups.elements();
        let correct = existing
            .iter()
            .filter(|node| node.to_serde_value().as_ref() == Some(&expected))
            .count();
        let managed = existing
            .iter()
            .filter(|node| {
                node.to_serde_value()
                    .and_then(|group| group.get("hooks").cloned())
                    .and_then(|hooks| hooks.as_array().cloned())
                    .is_some_and(|handlers| handlers.iter().any(is_managed_hook))
            })
            .count();
        if correct == 1 && managed == 1 {
            continue;
        }
        remove_managed_from_groups(&groups);
        groups.append(cst_input(expected));
    }
    prune_unknown_managed(&hooks, &known);
    Ok(root.to_string())
}

/// Removal is not limited to the current event set: older Kumu versions may
/// have registered events this one no longer knows about. Returns whether any
/// managed handler was removed.
fn remove_managed_hooks(hooks: &CstObject) -> bool {
    let mut removed = false;
    for property in hooks.properties() {
        let Some(groups) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        let event_removed = remove_managed_from_groups(&groups);
        removed |= event_removed;
        if event_removed && groups.elements().is_empty() {
            property.remove();
        }
    }
    removed
}

/// Repair also drops hooks for events this version no longer registers.
fn prune_unknown_managed(hooks: &CstObject, known: &[&str]) {
    for property in hooks.properties() {
        let Some(name) = property.decoded_name() else {
            continue;
        };
        if known.contains(&name.as_str()) {
            continue;
        }
        let Some(groups) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        if remove_managed_from_groups(&groups) && groups.elements().is_empty() {
            property.remove();
        }
    }
}

/// Cursor's user hooks are flat: an event maps straight to an array of
/// handlers, and a handler is the command itself — one level shallower than
/// Claude/Codex. It therefore gets its own read-modify-write.
pub(super) fn edit_cursor_hooks(text: &str, remove: bool) -> io::Result<String> {
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(io::Error::other)?;
    let object = root
        .object_value()
        .ok_or_else(|| io::Error::other("配置必须是 JSON 对象，未覆盖原文件"))?;
    if remove {
        let Some(hooks) = object.object_value("hooks") else {
            return Ok(text.into());
        };
        if !remove_cursor_handlers(&hooks) {
            return Ok(text.into());
        }
        if hooks.properties().is_empty()
            && let Some(property) = object.get("hooks")
        {
            property.remove();
        }
        return Ok(root.to_string());
    }
    // Cursor requires a schema version; add it only when the file is new.
    if object.get("version").is_none() {
        object.append("version", CstInputValue::Number("1".into()));
    }
    let hooks = object
        .object_value_or_create("hooks")
        .ok_or_else(|| io::Error::other("hooks 必须是对象，未覆盖原文件"))?;
    let events = cursor_events();
    let known: Vec<&str> = events.iter().map(|(event, _)| *event).collect();
    for (event, kind) in events {
        let expected = cursor_hook(kind);
        let handlers = hooks
            .array_value_or_create(event)
            .ok_or_else(|| io::Error::other(format!("hooks.{event} 必须是数组，未覆盖原文件")))?;
        let existing = handlers.elements();
        let correct = existing
            .iter()
            .filter(|node| node.to_serde_value().as_ref() == Some(&expected))
            .count();
        let managed = existing
            .iter()
            .filter(|node| node.to_serde_value().as_ref().is_some_and(is_managed_hook))
            .count();
        if correct == 1 && managed == 1 {
            continue;
        }
        remove_managed_from_event(&handlers);
        handlers.append(cst_input(expected));
    }
    prune_unknown_cursor(&hooks, &known);
    Ok(root.to_string())
}

/// `stop` is the only Cursor event that reports the end of a turn. Cursor has
/// no permission-prompt event: `beforeShellExecution` is a permission gate, and
/// a malformed response blocks the action, so it must not carry a notification.
pub(super) fn cursor_events() -> Vec<(&'static str, &'static str)> {
    vec![("stop", "complete")]
}

pub(super) fn cursor_hook(kind: &str) -> Value {
    json!({ "command": notify_command("cursor", kind), "timeout": 3 })
}

/// Antigravity's global `hooks.json` maps top-level hook names directly to
/// event objects (no outer `hooks` wrapper). Kumu owns only its own named
/// entry so user hooks in the same file stay untouched.
fn edit_agy_hooks(text: &str, remove: bool) -> io::Result<String> {
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(io::Error::other)?;
    let object = root
        .object_value()
        .ok_or_else(|| io::Error::other("配置必须是 JSON 对象，未覆盖原文件"))?;
    if remove {
        let Some(hooks) = object.object_value(AGY_HOOK_NAME) else {
            return Ok(text.into());
        };
        if !remove_agy_handlers(&hooks) {
            return Ok(text.into());
        }
        if hooks.properties().is_empty()
            && let Some(property) = object.get(AGY_HOOK_NAME)
        {
            property.remove();
        }
        return Ok(root.to_string());
    }
    let hooks = object
        .object_value_or_create(AGY_HOOK_NAME)
        .ok_or_else(|| io::Error::other(format!("{AGY_HOOK_NAME} 必须是对象，未覆盖原文件")))?;
    if let Some(enabled) = hooks.get("enabled")
        && enabled.value().and_then(|value| value.to_serde_value()) == Some(Value::Bool(false))
    {
        enabled.remove();
    }
    let events = agy_events();
    let known: Vec<&str> = events.iter().map(|(event, _)| *event).collect();
    for (event, kind) in events {
        let expected = agy_hook(kind);
        let handlers = hooks.array_value_or_create(event).ok_or_else(|| {
            io::Error::other(format!("{AGY_HOOK_NAME}.{event} 必须是数组，未覆盖原文件"))
        })?;
        let existing = handlers.elements();
        let correct = existing
            .iter()
            .filter(|node| node.to_serde_value().as_ref() == Some(&expected))
            .count();
        let managed = existing
            .iter()
            .filter(|node| node.to_serde_value().as_ref().is_some_and(is_managed_hook))
            .count();
        if correct == 1 && managed == 1 {
            continue;
        }
        remove_managed_from_event(&handlers);
        handlers.append(cst_input(expected));
    }
    prune_unknown_agy(&hooks, &known);
    Ok(root.to_string())
}

/// Antigravity's `PreToolUse` is a permission gate whose stdout requires a
/// `decision`, so Kumu does not attach notifications to it. `SessionStart`
/// reports the conversation id silently when a session opens;
/// `PreInvocation` runs before every model call and marks the turn running
/// (Antigravity has no prompt-submit event); `Stop` reports turn completion.
fn agy_events() -> Vec<(&'static str, &'static str)> {
    vec![
        ("SessionStart", "session"),
        ("PreInvocation", "start"),
        ("Stop", "complete"),
    ]
}

pub(super) fn agy_hook(kind: &str) -> Value {
    // Antigravity parses every command hook's stdout as JSON, including when
    // the same global file runs in Antigravity IDE/App outside Kumu. Always
    // emit `{}` so the hook stays a valid no-op everywhere.
    let command = format!(
        "if [ \"${{KUMU_TERMINAL:-}}\" = 1 ] && [ -n \"${{KUMU_APP_BINARY:-}}\" ]; then \"$KUMU_APP_BINARY\" --kumu-notify agy {kind}; fi; printf '{{}}'{MARKER}"
    );
    json!({ "type": "command", "command": command, "timeout": 3 })
}

fn remove_agy_handlers(hooks: &CstObject) -> bool {
    let mut removed = false;
    for property in hooks.properties() {
        let Some(entries) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        let event_removed =
            remove_managed_from_event(&entries) | remove_managed_from_groups(&entries);
        removed |= event_removed;
        if entries.elements().is_empty() {
            property.remove();
        }
    }
    removed
}

fn prune_unknown_agy(hooks: &CstObject, known: &[&str]) {
    for property in hooks.properties() {
        let Some(name) = property.decoded_name() else {
            continue;
        };
        if known.contains(&name.as_str()) {
            continue;
        }
        let Some(entries) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        if (remove_managed_from_event(&entries) | remove_managed_from_groups(&entries))
            && entries.elements().is_empty()
        {
            property.remove();
        }
    }
}

fn remove_cursor_handlers(hooks: &CstObject) -> bool {
    let mut removed = false;
    for property in hooks.properties() {
        let Some(handlers) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        let event_removed = remove_managed_from_event(&handlers);
        removed |= event_removed;
        if event_removed && handlers.elements().is_empty() {
            property.remove();
        }
    }
    removed
}

fn remove_managed_from_event(handlers: &CstArray) -> bool {
    let managed: Vec<_> = handlers
        .elements()
        .into_iter()
        .filter(|node| node.to_serde_value().as_ref().is_some_and(is_managed_hook))
        .collect();
    if managed.is_empty() {
        return false;
    }
    for handler in managed {
        handler.remove();
    }
    true
}

fn prune_unknown_cursor(hooks: &CstObject, known: &[&str]) {
    for property in hooks.properties() {
        let Some(name) = property.decoded_name() else {
            continue;
        };
        if known.contains(&name.as_str()) {
            continue;
        }
        let Some(handlers) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        if remove_managed_from_event(&handlers) && handlers.elements().is_empty() {
            property.remove();
        }
    }
}

fn remove_managed_from_groups(groups: &CstArray) -> bool {
    let mut removed = false;
    for group in groups.elements() {
        let Some(handlers) = group
            .as_object()
            .and_then(|object| object.array_value("hooks"))
        else {
            continue;
        };
        let elements = handlers.elements();
        let managed: Vec<_> = elements
            .iter()
            .filter(|node| node.to_serde_value().as_ref().is_some_and(is_managed_hook))
            .cloned()
            .collect();
        if managed.is_empty() {
            continue;
        }
        removed = true;
        if managed.len() == elements.len() {
            group.remove();
        } else {
            for handler in managed {
                handler.remove();
            }
        }
    }
    removed
}

fn is_managed_hook(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.ends_with(MARKER))
}

/// Convert generated hook definitions to CST inserts; user nodes stay untouched.
fn cst_input(value: Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value),
        Value::Array(values) => CstInputValue::Array(values.into_iter().map(cst_input).collect()),
        Value::Object(values) => CstInputValue::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, cst_input(value)))
                .collect(),
        ),
    }
}

/// Compare a parsed hook file against what this version registers. `nested`
/// says whether handlers sit inside a group's `hooks` array (Claude/Codex/Grok)
/// or are the event's entries themselves (Cursor).
pub(super) fn hooks_status(
    value: &Value,
    install: &[(&'static str, Value)],
    nested: bool,
) -> Status {
    let status = hooks_object_status(value.get("hooks"), install, nested);
    // Cursor rejects hooks files that lack a schema version.
    if !nested && value.get("version").is_none() && status != Status::Missing {
        return Status::NeedsRepair;
    }
    status
}

pub(super) fn agy_hooks_status(value: &Value) -> Status {
    let install: Vec<(&'static str, Value)> = agy_events()
        .into_iter()
        .map(|(event, kind)| (event, agy_hook(kind)))
        .collect();
    let entry = value.get(AGY_HOOK_NAME);
    let status = hooks_object_status(entry, &install, false);
    if status == Status::Registered
        && entry
            .and_then(|hook| hook.get("enabled"))
            .and_then(Value::as_bool)
            == Some(false)
    {
        return Status::NeedsRepair;
    }
    status
}

fn hooks_object_status(
    hooks: Option<&Value>,
    install: &[(&'static str, Value)],
    nested: bool,
) -> Status {
    let known: Vec<&str> = install.iter().map(|(event, _)| *event).collect();
    let mut found = false;
    let mut complete = true;
    for (event, expected) in install {
        let groups = hooks
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array);
        let correct = groups
            .map(|entries| entries.iter().filter(|entry| *entry == expected).count())
            .unwrap_or(0);
        let managed = groups
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| {
                        if nested {
                            entry
                                .get("hooks")
                                .and_then(Value::as_array)
                                .is_some_and(|handlers| handlers.iter().any(is_managed_hook))
                        } else {
                            is_managed_hook(entry)
                        }
                    })
                    .count()
            })
            .unwrap_or(0);
        complete &= correct == 1 && managed == 1;
        found |= event_handlers_in(hooks, event, nested)
            .iter()
            .any(is_managed_hook);
    }
    // A managed handler under an event this version no longer registers still
    // fires in the Agent; ask for a repair so install can prune it.
    let stale = hooks.and_then(Value::as_object).is_some_and(|map| {
        map.keys().any(|event| {
            !known.contains(&event.as_str())
                && event_handlers_in(hooks, event, nested)
                    .iter()
                    .any(is_managed_hook)
        })
    });
    if complete && !stale {
        Status::Registered
    } else if found || stale {
        Status::NeedsRepair
    } else {
        Status::Missing
    }
}

fn event_handlers_in(hooks: Option<&Value>, event: &str, nested: bool) -> Vec<Value> {
    let Some(groups) = hooks
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    groups
        .iter()
        .flat_map(|group| {
            if nested {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            } else {
                vec![group.clone()]
            }
        })
        .collect()
}

pub(super) fn update_hook_file(path: &Path, agent: &str, remove: bool) -> io::Result<()> {
    // Preserve a user's symlink to a dotfiles repository.
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(io::Error::other("配置链接的目标不存在，未覆盖原链接"));
            }
            path.to_path_buf()
        }
        Err(error) => return Err(error),
    };
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if remove && original.is_none() {
        return Ok(());
    }
    let updated = edit_hooks(
        original.as_deref().unwrap_or({
            // Cursor rejects a hooks file without a schema version.
            if agent == "cursor" {
                "{\"version\": 1}\n"
            } else {
                "{}\n"
            }
        }),
        agent,
        remove,
    )?;
    if original.as_deref() == Some(updated.as_str()) {
        return Ok(());
    }
    let parent = path.parent().expect("hook directory");
    std::fs::create_dir_all(parent)?;
    let temporary = temporary_path(&path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        if let Ok(metadata) = std::fs::metadata(&path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
        // Best effort against a concurrent editor; distinguish NotFound from IO errors.
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if current != original {
            return Err(io::Error::other("Agent 配置已被其他程序修改，请重试"));
        }
        std::fs::rename(&temporary, &path)?;
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(super) fn temporary_path(path: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let hidden = format!(".{name}.kumu-{}-{nonce}-{seq}.tmp", std::process::id());
    match path.parent() {
        Some(parent) => parent.join(hidden),
        None => PathBuf::from(hidden),
    }
}
