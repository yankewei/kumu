//! Persistent agent registration, independent of how the user launches a CLI.

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Value, json};

const MARKER: &str = " # kumu-notifications";

#[derive(Clone, Copy, PartialEq, Eq)]
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
        _ => return Err(io::Error::other("不支持的 Agent")),
    };
    Ok(Paths {
        assets,
        managed,
        registration,
    })
}

fn hook_groups(agent: &str) -> Vec<(&'static str, Value)> {
    let mut events = vec![
        ("Stop", None, "complete"),
        ("PermissionRequest", None, "permission"),
    ];
    if agent == "claude" {
        events.extend([
            ("PreToolUse", Some("AskUserQuestion|ExitPlanMode"), "input"),
            // PermissionRequest already fires the moment a prompt appears;
            // Notification's permission_prompt/idle_prompt would later repeat
            // the same turn. Keep only MCP elicitation forms, which have no
            // PermissionRequest event of their own.
            ("Notification", Some("elicitation_dialog"), "input"),
        ]);
    } else {
        events.push(("PreToolUse", Some("^request_user_input$"), "input"));
    }
    events.into_iter().map(|(event, matcher, kind)| {
        let command = format!("if [ \"${{KUMU_TERMINAL:-}}\" = 1 ] && [ -n \"${{KUMU_APP_BINARY:-}}\" ]; then \"$KUMU_APP_BINARY\" --kumu-notify {agent} {kind}; fi{MARKER}");
        let mut group = json!({"hooks": [{"type": "command", "command": command, "timeout": 3}]});
        if let Some(matcher) = matcher { group["matcher"] = json!(matcher); }
        (event, group)
    }).collect()
}

/// Preserve JSONC comments, user hooks, and unchanged hook hashes during repair.
fn edit_hooks(text: &str, agent: &str, remove: bool) -> io::Result<String> {
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

pub fn status(agent: &str) -> io::Result<Status> {
    let paths = paths(agent)?;
    if matches!(agent, "codex" | "claude") {
        let text = match std::fs::read_to_string(&paths.registration) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Status::Missing),
            Err(error) => return Err(error),
        };
        let root = CstRootNode::parse(&text, &ParseOptions::default()).map_err(io::Error::other)?;
        let value = root
            .to_serde_value()
            .ok_or_else(|| io::Error::other("无效的 Agent 配置"))?;
        let groups_to_check = hook_groups(agent);
        let known: Vec<&str> = groups_to_check.iter().map(|(event, _)| *event).collect();
        let mut found = false;
        let mut complete = true;
        for (event, expected) in groups_to_check {
            let groups = value
                .get("hooks")
                .and_then(|hooks| hooks.get(event))
                .and_then(Value::as_array);
            complete &= groups.is_some_and(|groups| groups.iter().any(|group| group == &expected));
            found |= groups.is_some_and(|groups| {
                groups
                    .iter()
                    .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .any(is_managed_hook)
            });
        }
        // A managed handler under an event this version no longer registers
        // still fires in the Agent; ask for a repair so install can prune it.
        let stale = value
            .get("hooks")
            .and_then(Value::as_object)
            .is_some_and(|hooks| {
                hooks.iter().any(|(event, groups)| {
                    !known.contains(&event.as_str())
                        && groups.as_array().is_some_and(|groups| {
                            groups
                                .iter()
                                .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                                .flatten()
                                .any(is_managed_hook)
                        })
                })
            });
        return Ok(if complete && !stale {
            Status::Registered
        } else if found || stale {
            Status::NeedsRepair
        } else {
            Status::Missing
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
    if matches!(agent, "codex" | "claude") {
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
    if matches!(agent, "codex" | "claude") {
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
    let updated = edit_hooks(original.as_deref().unwrap_or("{}\n"), agent, remove)?;
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
    let sequence = format!("\x1b]777;notify;{agent};{message}\x07");
    match agent.as_str() {
        // Claude runs command hooks in their own session without a controlling
        // terminal. Its documented channel is `terminalSequence` on stdout.
        "claude" => {
            let output = json!({ "terminalSequence": sequence });
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(output.to_string().as_bytes());
            let _ = stdout.write_all(b"\n");
        }
        // Codex also detaches hooks, so write to the explicit PTY path Kumu
        // inherited into the process rather than `/dev/tty`.
        "codex" => {
            if let Some(mut tty) = open_notification_tty() {
                let _ = tty.write_all(sequence.as_bytes());
            }
        }
        _ => {}
    }
    Some(0)
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
        for agent in ["codex", "claude"] {
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
