//! Persistent agent registration, independent of how the user launches a CLI.

use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Value, json};

use super::launch::{SESSION_PREFIX, clean_session};

const MARKER: &str = " # kumu-notifications";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Missing,
    Registered,
    NeedsRepair,
}

struct Paths {
    assets: PathBuf,
    managed: PathBuf,
    registration: PathBuf,
}

fn paths(agent: &str) -> io::Result<Paths> {
    let home = crate::db::db_path()
        .parent()
        .and_then(Path::parent)
        .expect("home directory")
        .to_path_buf();
    // Pi and Oh My Pi load the same extension API. They share one installed
    // module so a common `PI_CODING_AGENT_DIR` cannot make the two
    // registrations fight over a single symlink.
    let managed = home.join(".kumu/agent-integration");
    let assets = managed.join(if matches!(agent, "pi" | "omp") {
        "pi"
    } else {
        agent
    });
    let registration = match agent {
        "codex" => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("hooks.json"),
        "claude" => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"))
            .join("settings.json"),
        "pi" => std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi/agent"))
            .join("extensions/kumu-notifications"),
        "omp" => std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("PI_CONFIG_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".omp"))
                    .join("agent")
            })
            .join("extensions/kumu-notifications"),
        "opencode" => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("opencode/plugins");
            config.join("kumu-notifications")
        }
        // Cursor's own config directory: CURSOR_CONFIG_DIR, else
        // $XDG_CONFIG_HOME/cursor, else ~/.cursor. Its user hooks live in one
        // flat `hooks.json` at the top of that directory.
        "cursor" => std::env::var_os("CURSOR_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("cursor"))
            })
            .unwrap_or_else(|| home.join(".cursor"))
            .join("hooks.json"),
        // Grok Build reads personal hooks from ~/.grok/hooks/*.json. Kumu keeps
        // its own file so installing never has to rewrite a user's hook file.
        "grok" => home.join(".grok/hooks/kumu-notifications.json"),
        _ => return Err(io::Error::other("不支持的 Agent")),
    };
    Ok(Paths {
        assets,
        managed,
        registration,
    })
}

fn hook_groups(agent: &str) -> Vec<(&'static str, Value)> {
    let events: Vec<(&'static str, Option<&'static str>, &'static str)> = match agent {
        // PermissionRequest already fires the moment a prompt appears;
        // Notification's permission_prompt/idle_prompt would later repeat the
        // same turn. Keep only MCP elicitation forms, which have no
        // PermissionRequest event of their own.
        "claude" => vec![
            ("Stop", None, "complete"),
            ("PermissionRequest", None, "permission"),
            ("PreToolUse", Some("AskUserQuestion|ExitPlanMode"), "input"),
            ("Notification", Some("elicitation_dialog"), "input"),
        ],
        // Grok Build has no PermissionRequest; its Notification event is the
        // channel that carries a permission prompt or a question.
        "grok" => vec![("Stop", None, "complete"), ("Notification", None, "input")],
        _ => vec![
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
fn edit_hooks(text: &str, agent: &str, remove: bool) -> io::Result<String> {
    if agent == "cursor" {
        return edit_cursor_hooks(text, remove);
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
        if groups.elements().is_empty() {
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
fn edit_cursor_hooks(text: &str, remove: bool) -> io::Result<String> {
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
fn cursor_events() -> Vec<(&'static str, &'static str)> {
    vec![("stop", "complete")]
}

fn cursor_hook(kind: &str) -> Value {
    json!({ "command": notify_command("cursor", kind), "timeout": 3 })
}

fn remove_cursor_handlers(hooks: &CstObject) -> bool {
    let mut removed = false;
    for property in hooks.properties() {
        let Some(handlers) = property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        removed |= remove_managed_from_event(&handlers);
        if handlers.elements().is_empty() {
            property.remove();
        }
    }
    removed
}

fn remove_managed_from_event(handlers: &jsonc_parser::cst::CstArray) -> bool {
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

fn remove_managed_from_groups(groups: &jsonc_parser::cst::CstArray) -> bool {
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

/// Native discovery packages reuse the notification bridge; Pi and Oh My Pi
/// share one module because they load the same extension API.
fn asset_files(agent: &str) -> Vec<(&'static str, String)> {
    let mut files = vec![(
        "package.json",
        include_str!("integration/package.json").to_string(),
    )];
    if agent == "opencode" {
        files.extend([
            ("notify.js", include_str!("integration/notify.js").into()),
            ("v1.js", include_str!("integration/opencode-v1.js").into()),
            (
                "extension/index.js",
                "import { KumuNotifications } from '../v1.js';\nexport default { id: 'kumu.notifications', setup() {}, server: KumuNotifications };\n".into(),
            ),
            (
                "extension/package.json",
                "{\"type\":\"module\",\"exports\":{\".\":\"./index.js\",\"./tui\":\"./tui.js\"}}\n"
                    .into(),
            ),
            (
                "extension/tui.js",
                include_str!("integration/opencode/tui.js").into(),
            ),
        ]);
    } else {
        files.extend([
            ("pi.js", include_str!("integration/pi.js").into()),
            ("notify.js", include_str!("integration/notify.js").into()),
            (
                "extension/index.ts",
                "import register from '../pi.js';\nexport default pi => register(pi);\n".into(),
            ),
        ]);
    }
    files
}

/// Compare a parsed hook file against what this version registers. `nested`
/// says whether handlers sit inside a group's `hooks` array (Claude/Codex/Grok)
/// or are the event's entries themselves (Cursor).
fn hooks_status(value: &Value, install: &[(&'static str, Value)], nested: bool) -> Status {
    let known: Vec<&str> = install.iter().map(|(event, _)| *event).collect();
    let mut found = false;
    let mut complete = true;
    for (event, expected) in install {
        let groups = value
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array);
        // Compare the entry itself, so a user-added handler inside the same
        // nested group does not pass as Kumu's registration.
        complete &= groups.is_some_and(|groups| groups.iter().any(|entry| entry == expected));
        found |= event_handlers(value, event, nested)
            .iter()
            .any(is_managed_hook);
    }
    // A managed handler under an event this version no longer registers still
    // fires in the Agent; ask for a repair so install can prune it.
    let stale = value
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks.keys().any(|event| {
                !known.contains(&event.as_str())
                    && event_handlers(value, event, nested)
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

fn event_handlers(value: &Value, event: &str, nested: bool) -> Vec<Value> {
    let Some(groups) = value
        .get("hooks")
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

pub fn status(agent: &str) -> io::Result<Status> {
    let paths = paths(agent)?;
    if matches!(agent, "codex" | "claude" | "grok" | "cursor") {
        let text = match std::fs::read_to_string(&paths.registration) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Status::Missing),
            Err(error) => return Err(error),
        };
        let root = CstRootNode::parse(&text, &ParseOptions::default()).map_err(io::Error::other)?;
        let value = root
            .to_serde_value()
            .ok_or_else(|| io::Error::other("无效的 Agent 配置"))?;
        return Ok(if agent == "cursor" {
            let install: Vec<(&'static str, Value)> = cursor_events()
                .into_iter()
                .map(|(event, kind)| (event, cursor_hook(kind)))
                .collect();
            hooks_status(&value, &install, false)
        } else {
            hooks_status(&value, &hook_groups(agent), true)
        });
    }
    let target = paths.assets.join("extension");
    match std::fs::read_link(&paths.registration) {
        Ok(current) if current == target => {
            let current = paths.registration.exists()
                && asset_files(agent).iter().all(|(file, expected)| {
                    std::fs::read_to_string(paths.assets.join(file))
                        .ok()
                        .as_deref()
                        == Some(expected.as_str())
                });
            Ok(if current {
                Status::Registered
            } else {
                Status::NeedsRepair
            })
        }
        // A link from an older Kumu layout can be repaired in place.
        Ok(current) if current.starts_with(&paths.managed) => Ok(Status::NeedsRepair),
        Ok(_) => Err(io::Error::other(format!(
            "接入路径已被其他链接占用：{}",
            paths.registration.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Status::Missing),
        Err(error) => Err(error),
    }
}

pub fn install(agent: &str) -> io::Result<()> {
    let paths = paths(agent)?;
    if matches!(agent, "codex" | "claude" | "grok" | "cursor") {
        return update_hook_file(&paths.registration, agent, false);
    }
    for (relative, contents) in asset_files(agent) {
        let file = paths.assets.join(relative);
        std::fs::create_dir_all(file.parent().expect("asset directory"))?;
        if std::fs::read_to_string(&file).ok().as_deref() != Some(contents.as_str()) {
            std::fs::write(file, contents)?;
        }
    }
    register_link(
        &paths.registration,
        &paths.assets.join("extension"),
        &paths.managed,
    )?;
    Ok(())
}

pub fn uninstall(agent: &str) -> io::Result<()> {
    let paths = paths(agent)?;
    if matches!(agent, "codex" | "claude" | "grok" | "cursor") {
        return update_hook_file(&paths.registration, agent, true);
    }
    remove_link(
        &paths.registration,
        &paths.assets.join("extension"),
        &paths.managed,
    )
}

fn register_link(entry: &Path, target: &Path, managed: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(entry) {
        Ok(_) if std::fs::read_link(entry).ok().as_deref() == Some(target) => return Ok(()),
        Ok(_) => {
            let previous = std::fs::read_link(entry).ok();
            if !previous
                .as_deref()
                .is_some_and(|path| path.starts_with(managed))
            {
                return Err(io::Error::other(format!(
                    "接入路径已有其他文件，未覆盖：{}",
                    entry.display()
                )));
            }
            // Replace Kumu's own older layout instead of reporting a conflict.
            std::fs::remove_file(entry)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(entry.parent().expect("registration directory"))?;
    symlink(target, entry)
}

fn remove_link(entry: &Path, target: &Path, managed: &Path) -> io::Result<()> {
    match std::fs::read_link(entry) {
        Ok(current) if current == target => std::fs::remove_file(entry),
        Ok(current) if current.starts_with(managed) => std::fs::remove_file(entry),
        Ok(_) => Err(io::Error::other(format!(
            "接入路径属于其他链接，未删除：{}",
            entry.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn update_hook_file(path: &Path, agent: &str, remove: bool) -> io::Result<()> {
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
    std::fs::create_dir_all(path.parent().expect("hook directory"))?;
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
        // Best effort against a concurrent editor; rename itself is atomic.
        if std::fs::read_to_string(&path).ok() != original {
            return Err(io::Error::other("Agent 配置已被其他程序修改，请重试"));
        }
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("kumu-{}-{nonce}.tmp", std::process::id()))
}

/// Called by persistent hooks; the CLI returns before GPUI is initialized.
pub fn run_cli() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--kumu-notify") {
        return None;
    }
    let agent = args.next().unwrap_or_default();
    let message = match args.next().as_deref() {
        Some("complete") => "本轮任务已结束",
        Some("permission") => "需要你确认操作权限",
        _ => "需要你输入、确认或选择",
    };
    if std::env::var("KUMU_TERMINAL").as_deref() != Ok("1") {
        return Some(0);
    }
    // Hooks run with the event's JSON on stdin, which is where Claude, Codex
    // and Cursor put the session id. The session update is another OSC on the
    // same channel, framed as a notification so the pane can tell it apart.
    let mut sequence = String::new();
    if let Some(session) = hook_payload().as_ref().and_then(session_id)
        && let Some(update) = session_sequence(&agent, &session)
    {
        sequence.push_str(&update);
    }
    sequence.push_str(&format!("\x1b]777;notify;{agent};{message}\x07"));
    match agent.as_str() {
        // Claude runs command hooks in their own session without a controlling
        // terminal. Its documented channel is `terminalSequence` on stdout.
        "claude" => {
            let output = json!({ "terminalSequence": sequence });
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(output.to_string().as_bytes());
            let _ = stdout.write_all(b"\n");
        }
        // Codex, Grok and Cursor detach hooks, so write to the explicit PTY
        // path Kumu inherited into the process rather than `/dev/tty`.
        "codex" | "grok" | "cursor" => {
            if let Some(mut tty) = open_notification_tty() {
                let _ = tty.write_all(sequence.as_bytes());
            }
        }
        _ => {}
    }
    Some(0)
}

/// The hook event as JSON, or `None` when stdin is a terminal or stays silent.
///
/// Hooks close stdin after writing, but a shell this binary is run from by hand
/// has no payload at all: the wait is bounded so an interactive run cannot
/// hang the hook's timeout.
fn hook_payload() -> Option<Value> {
    use std::io::IsTerminal;
    use std::sync::mpsc;
    use std::time::Duration;

    if io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut payload = String::new();
        let _ = io::stdin().read_to_string(&mut payload);
        let _ = tx.send(payload);
    });
    let payload = rx.recv_timeout(Duration::from_millis(300)).ok()?;
    serde_json::from_str(&payload).ok()
}

/// The OSC that reports a conversation id, framed as a notification so it
/// travels the channel every hook already writes to.
fn session_sequence(agent: &str, session: &str) -> Option<String> {
    let session = clean_session(session)?;
    Some(format!(
        "\x1b]777;notify;{agent};{SESSION_PREFIX}{session}\x07"
    ))
}

/// The conversation id in a hook payload. Every shape Kumu registers uses
/// `session_id`; the other spellings cover CLIs that name it differently.
fn session_id(payload: &Value) -> Option<String> {
    const KEYS: [&str; 6] = [
        "session_id",
        "sessionId",
        "conversation_id",
        "conversationId",
        "thread_id",
        "threadId",
    ];
    KEYS.iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn open_notification_tty() -> Option<std::fs::File> {
    let path = std::env::var_os("KUMU_TTY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/tty"));
    std::fs::OpenOptions::new().write(true).open(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_conversation_id_travels_as_a_silent_osc() {
        assert_eq!(
            session_sequence("codex", "abc-1").as_deref(),
            Some("\x1b]777;notify;codex;kumu-session:abc-1\x07")
        );
        assert!(session_sequence("codex", "a;b").is_none());
        assert!(session_sequence("codex", "a\x07b").is_none());
    }

    #[test]
    fn a_hook_payloads_session_id_is_found_under_its_known_names() {
        assert_eq!(
            session_id(&json!({"session_id": "a"})).as_deref(),
            Some("a")
        );
        assert_eq!(
            session_id(&json!({"conversation_id": "b"})).as_deref(),
            Some("b")
        );
        assert_eq!(session_id(&json!({"threadId": "c"})).as_deref(), Some("c"));
        assert_eq!(session_id(&json!({"session_id": 3})), None);
        assert_eq!(session_id(&json!({})), None);
    }

    fn fixture_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kumu-hooks-{}-{nonce}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("fixture directory");
        dir
    }

    #[test]
    fn hook_registration_preserves_comments_user_hooks_and_hook_hashes() {
        let original = r#"{
  // Keep my settings and this comment.
  "model": "my-model",
  "hooks": {
    "Stop": [{"hooks": [{"type": "command", "command": "my-existing-hook"}]}],
  },
}
"#;
        for agent in ["codex", "claude", "grok"] {
            let registered = edit_hooks(original, agent, false).expect("register");
            assert!(registered.contains("// Keep my settings and this comment."));
            assert!(registered.contains("my-existing-hook"));
            assert_eq!(
                registered,
                edit_hooks(&registered, agent, false).expect("repair"),
                "repair must not change trusted definitions"
            );
            let removed = edit_hooks(&registered, agent, true).expect("remove");
            assert!(!removed.contains(MARKER));
            assert!(removed.contains("my-existing-hook"));
            assert!(removed.contains("// Keep my settings and this comment."));
            let root =
                CstRootNode::parse(&removed, &ParseOptions::default()).expect("valid config");
            assert_eq!(root.to_serde_value().expect("value")["model"], "my-model");
        }
    }

    #[test]
    fn removal_preserves_foreign_handlers_added_to_a_managed_group() {
        let mut group = hook_groups("claude")[0].1.clone();
        group["hooks"]
            .as_array_mut()
            .expect("hooks")
            .push(json!({"type":"command", "command":"my-custom-hook"}));
        let original = json!({"hooks":{"Stop":[group]}}).to_string();
        let removed = edit_hooks(&original, "claude", true).expect("remove");
        assert!(!removed.contains(MARKER));
        assert!(removed.contains("my-custom-hook"));
    }

    #[test]
    fn removal_cleans_events_this_version_no_longer_registers() {
        let stale = hook_groups("claude")[0].1.clone();
        let original = json!({"hooks":{
            "SubagentStop": [stale],
            "Stop": [hook_groups("claude")[0].1.clone()],
        }})
        .to_string();
        let removed = edit_hooks(&original, "claude", true).expect("remove");
        assert!(!removed.contains(MARKER), "{removed}");
        let root = CstRootNode::parse(&removed, &ParseOptions::default()).expect("valid config");
        let value = root.to_serde_value().expect("value");
        assert!(value.get("hooks").is_none(), "{value}");
    }

    #[test]
    fn repair_prunes_events_this_version_no_longer_registers() {
        let stale = hook_groups("claude")[0].1.clone();
        let original = json!({"hooks":{"SubagentStop":[stale]}}).to_string();
        let repaired = edit_hooks(&original, "claude", false).expect("repair");
        assert!(!repaired.contains("SubagentStop"), "{repaired}");
        for event in ["Stop", "PermissionRequest", "PreToolUse", "Notification"] {
            assert!(repaired.contains(event), "{event} missing: {repaired}");
        }
    }

    #[test]
    fn claude_notification_does_not_repeat_permission_or_idle_prompts() {
        let notification = hook_groups("claude")
            .into_iter()
            .find(|(event, _)| *event == "Notification")
            .expect("notification group")
            .1;
        assert_eq!(notification["matcher"], "elicitation_dialog");
    }

    #[test]
    fn grok_registers_completion_and_attention_events() {
        let events: Vec<&str> = hook_groups("grok")
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert_eq!(events, ["Stop", "Notification"]);
    }

    /// Cursor's file is flat and versioned; both must survive a round trip that
    /// only touches Kumu's own handler.
    #[test]
    fn cursor_hooks_keep_the_schema_version_and_foreign_handlers() {
        let original = r#"{
  // keep my comment
  "hooks": {
    "stop": [{"command": "my-stop-hook"}],
    "beforeShellExecution": [{"command": "my-gate", "timeout": 5}],
  },
}
"#;
        let registered = edit_cursor_hooks(original, false).expect("register");
        assert!(registered.contains("my-stop-hook"), "{registered}");
        assert!(registered.contains("my-gate"), "{registered}");
        assert!(registered.contains(MARKER), "{registered}");
        assert!(registered.contains("\"version\": 1"), "{registered}");
        assert_eq!(
            registered,
            edit_cursor_hooks(&registered, false).expect("repair"),
            "repair must not change trusted handlers"
        );
        let removed = edit_cursor_hooks(&registered, true).expect("remove");
        assert!(!removed.contains(MARKER), "{removed}");
        assert!(removed.contains("my-stop-hook"), "{removed}");
        assert!(removed.contains("my-gate"), "{removed}");
        assert!(removed.contains("// keep my comment"), "{removed}");
    }

    #[test]
    fn cursor_registration_adds_a_missing_schema_version() {
        let registered = edit_cursor_hooks("{}\n", false).expect("register");
        assert!(registered.contains("\"version\": 1"), "{registered}");
        let removed = edit_cursor_hooks(&registered, true).expect("remove");
        assert!(!removed.contains(MARKER), "{removed}");
    }

    /// Both hook file shapes are recognised, so the settings row can tell an
    /// installed registration from one that is missing or stale.
    #[test]
    fn status_recognizes_nested_and_flat_hook_files() {
        let install = hook_groups("grok");
        let value = json!({"hooks": {
            "Stop": [install[0].1.clone()],
            "Notification": [install[1].1.clone()],
        }});
        assert_eq!(hooks_status(&value, &install, true), Status::Registered);

        let install: Vec<(&'static str, Value)> = cursor_events()
            .into_iter()
            .map(|(event, kind)| (event, cursor_hook(kind)))
            .collect();
        let value = json!({"version": 1, "hooks": {"stop": [cursor_hook("complete")]}});
        assert_eq!(hooks_status(&value, &install, false), Status::Registered);
        assert_eq!(hooks_status(&json!({}), &install, false), Status::Missing);
    }

    #[test]
    fn pi_and_omp_share_one_extension_and_label_themselves_at_runtime() {
        assert_eq!(asset_files("pi"), asset_files("omp"));
        let (_, index) = asset_files("pi")
            .into_iter()
            .find(|(file, _)| *file == "extension/index.ts")
            .expect("extension entry");
        assert!(index.contains("register(pi)"), "{index}");
    }

    #[test]
    fn invalid_configuration_is_never_overwritten() {
        let dir = fixture_dir();
        let path = dir.join("settings.json");
        for text in [
            "not JSON",
            r#"{"hooks":false}"#,
            r#"{"hooks":{"Stop":false}}"#,
        ] {
            std::fs::write(&path, text).expect("fixture");
            assert!(update_hook_file(&path, "claude", false).is_err());
            assert_eq!(std::fs::read_to_string(&path).expect("read"), text);
        }
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn registration_preserves_dotfile_symlinks_and_permissions() {
        let dir = fixture_dir();
        let actual = dir.join("dotfile.json");
        let link = dir.join("settings.json");
        std::fs::write(&actual, "{}\n").expect("fixture");
        std::fs::set_permissions(&actual, std::fs::Permissions::from_mode(0o640)).expect("mode");
        symlink(&actual, &link).expect("symlink");
        update_hook_file(&link, "codex", false).expect("register");
        assert_eq!(std::fs::read_link(&link).expect("still a symlink"), actual);
        assert_eq!(
            std::fs::metadata(&actual)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        update_hook_file(&link, "codex", true).expect("remove");
        assert!(
            !std::fs::read_to_string(&actual)
                .expect("read")
                .contains(MARKER)
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn native_registration_never_overwrites_or_removes_unrelated_entries() {
        let dir = fixture_dir();
        let entry = dir.join("extensions/kumu-notifications");
        let target = dir.join("managed");
        std::fs::create_dir(&target).expect("target");
        register_link(&entry, &target, &dir).expect("register");
        register_link(&entry, &target, &dir).expect("idempotent");
        remove_link(&entry, &target, &dir).expect("remove");
        std::fs::write(&entry, "my extension").expect("foreign file");
        assert!(register_link(&entry, &target, &dir).is_err());
        assert!(remove_link(&entry, &target, &dir).is_err());
        assert_eq!(
            std::fs::read_to_string(&entry).expect("foreign preserved"),
            "my extension"
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn native_registration_repairs_an_older_kumu_link() {
        let dir = fixture_dir();
        let entry = dir.join("extensions/kumu-notifications");
        let old = dir.join("old/extension");
        let target = dir.join("pi/extension");
        std::fs::create_dir_all(&old).expect("old target");
        std::fs::create_dir_all(&target).expect("target");
        register_link(&entry, &old, &dir).expect("old link");
        register_link(&entry, &target, &dir).expect("repair");
        assert_eq!(std::fs::read_link(&entry).expect("repaired link"), target);
        remove_link(&entry, &target, &dir).expect("remove");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
