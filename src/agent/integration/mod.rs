//! Persistent agent registration, independent of how the user launches a CLI.

mod cli;
mod hooks;
mod mcp;
mod skills;

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstRootNode;
use serde_json::Value;

pub use cli::{is_hook_notification_body, run_cli};
use hooks::{
    agy_hooks_status, cursor_events, cursor_hook, hook_groups, hooks_status, temporary_path,
    update_hook_file,
};
pub use mcp::{install_mcp, mcp_agents, mcp_status, mcp_wrapper_path, supports_mcp, uninstall_mcp};
pub use skills::{install_skills, skills_status, uninstall_skills};

pub(super) fn non_empty_env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn is_under_managed(path: &Path, managed: &Path) -> bool {
    !path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        && path.starts_with(managed)
}

fn is_hook_agent(agent: &str) -> bool {
    matches!(agent, "codex" | "claude" | "grok" | "cursor" | "agy")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Missing,
    Registered,
    NeedsRepair,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpState {
    Supported(Result<Status, String>),
    ExtensionPartial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationDetail {
    pub notify: Result<Status, String>,
    pub mcp: McpState,
    pub skills: Result<Status, String>,
}

impl IntegrationDetail {
    /// Whether any component is present, broken, or needs cleanup.
    pub fn has_any_registration(&self) -> bool {
        let notify_present = !matches!(self.notify, Ok(Status::Missing));
        let mcp_present = matches!(
            &self.mcp,
            McpState::Supported(Ok(Status::Registered | Status::NeedsRepair) | Err(_))
        );
        let skills_present = !matches!(self.skills, Ok(Status::Missing));
        notify_present || mcp_present || skills_present
    }
}

#[derive(Clone, Debug)]
pub struct ComponentOutcome {
    pub notify: Result<(), String>,
    pub mcp: Option<Result<(), String>>,
    pub skills: Result<(), String>,
}

impl ComponentOutcome {
    pub fn is_ok(&self) -> bool {
        self.notify.is_ok() && self.mcp.as_ref().is_none_or(Result::is_ok) && self.skills.is_ok()
    }

    pub fn into_io_result(self) -> io::Result<()> {
        let mut errors = Vec::new();
        if let Err(err) = self.notify {
            errors.push(format!("通知：{err}"));
        }
        if let Some(Err(err)) = self.mcp {
            errors.push(format!("MCP：{err}"));
        }
        if let Err(err) = self.skills {
            errors.push(format!("Skills：{err}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(errors.join("；")))
        }
    }
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
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::other("无法确定用户主目录（HOME 未设置）"))?
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
        "codex" => non_empty_env_path("CODEX_HOME")
            .unwrap_or_else(|| home.join(".codex"))
            .join("hooks.json"),
        "claude" => non_empty_env_path("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| home.join(".claude"))
            .join("settings.json"),
        "pi" => non_empty_env_path("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| home.join(".pi/agent"))
            .join("extensions/kumu-notifications"),
        "omp" => non_empty_env_path("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| {
                non_empty_env_path("PI_CONFIG_DIR")
                    .unwrap_or_else(|| home.join(".omp"))
                    .join("agent")
            })
            .join("extensions/kumu-notifications"),
        "opencode" => {
            let config = non_empty_env_path("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("opencode/plugins");
            config.join("kumu-notifications")
        }
        // Cursor's own config directory: CURSOR_CONFIG_DIR, else
        // $XDG_CONFIG_HOME/cursor, else ~/.cursor. Its user hooks live in one
        // flat `hooks.json` at the top of that directory.
        "cursor" => non_empty_env_path("CURSOR_CONFIG_DIR")
            .or_else(|| non_empty_env_path("XDG_CONFIG_HOME").map(|path| path.join("cursor")))
            .unwrap_or_else(|| home.join(".cursor"))
            .join("hooks.json"),
        // Grok Build reads personal hooks from ~/.grok/hooks/*.json. Kumu keeps
        // its own file so installing never has to rewrite a user's hook file.
        "grok" => home.join(".grok/hooks/kumu-notifications.json"),
        // Antigravity reads global hooks from ~/.gemini/config/hooks.json,
        // where each top-level key is a named hook group rather than a shared
        // `hooks` object.
        "agy" => home.join(".gemini/config/hooks.json"),
        _ => return Err(io::Error::other("不支持的 Agent")),
    };
    Ok(Paths {
        assets,
        managed,
        registration,
    })
}

/// Native discovery packages reuse the notification bridge; Pi and Oh My Pi
/// share one module because they load the same extension API.
fn asset_files(agent: &str) -> Vec<(&'static str, String)> {
    let mut files = vec![("package.json", include_str!("package.json").to_string())];
    if agent == "opencode" {
        files.extend([
            ("notify.js", include_str!("notify.js").into()),
            ("v1.js", include_str!("opencode-v1.js").into()),
            (
                "extension/index.js",
                "import { KumuNotifications } from '../v1.js';\nexport default { id: 'kumu.notifications', setup() {}, server: KumuNotifications };\n".into(),
            ),
            (
                "extension/package.json",
                "{\"type\":\"module\",\"exports\":{\".\":\"./index.js\",\"./tui\":\"./tui.js\"}}\n"
                    .into(),
            ),
            ("extension/tui.js", include_str!("opencode/tui.js").into()),
        ]);
    } else {
        files.extend([
            ("pi.js", include_str!("pi.js").into()),
            ("notify.js", include_str!("notify.js").into()),
            (
                "extension/index.ts",
                "import register from '../pi.js';\nexport default pi => register(pi);\n".into(),
            ),
        ]);
    }
    files
}

pub(super) fn notify_status(agent: &str) -> io::Result<Status> {
    let paths = paths(agent)?;
    if is_hook_agent(agent) {
        let text = match std::fs::read_to_string(&paths.registration) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Status::Missing),
            Err(error) => return Err(error),
        };
        let root = CstRootNode::parse(&text, &ParseOptions::default()).map_err(io::Error::other)?;
        let value = root
            .to_serde_value()
            .ok_or_else(|| io::Error::other("无效的 Agent 配置"))?;
        let hook_state = if agent == "cursor" {
            let install: Vec<(&'static str, Value)> = cursor_events()
                .into_iter()
                .map(|(event, kind)| (event, cursor_hook(kind)))
                .collect();
            hooks_status(&value, &install, false)
        } else if agent == "agy" {
            agy_hooks_status(&value)
        } else {
            hooks_status(&value, &hook_groups(agent), true)
        };
        return Ok(hook_state);
    }
    let target = paths.assets.join("extension");
    match std::fs::symlink_metadata(&paths.registration) {
        Ok(meta) if !meta.file_type().is_symlink() => {
            return Err(io::Error::other(format!(
                "接入路径已有其他文件，未覆盖：{}",
                paths.registration.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Status::Missing),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
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
        Ok(current) if is_under_managed(&current, &paths.managed) => Ok(Status::NeedsRepair),
        Ok(_) => Err(io::Error::other(format!(
            "接入路径已被其他链接占用：{}",
            paths.registration.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Status::Missing),
        Err(error) => Err(error),
    }
}

pub fn detailed_status(agent: &str) -> IntegrationDetail {
    let notify = notify_status(agent).map_err(|e| e.to_string());
    let mcp = if supports_mcp(agent) {
        McpState::Supported(mcp_status(agent).map_err(|e| e.to_string()))
    } else {
        McpState::ExtensionPartial
    };
    let skills = skills_status(agent).map_err(|e| e.to_string());
    IntegrationDetail {
        notify,
        mcp,
        skills,
    }
}

fn aggregate_status(detail: &IntegrationDetail) -> io::Result<Status> {
    let notify = detail
        .notify
        .as_ref()
        .copied()
        .map_err(|err| io::Error::other(err.clone()))?;
    let skills = detail
        .skills
        .as_ref()
        .copied()
        .map_err(|err| io::Error::other(err.clone()))?;
    let mut states = vec![notify, skills];
    if let McpState::Supported(ref mcp) = detail.mcp {
        let mcp = mcp
            .as_ref()
            .copied()
            .map_err(|err| io::Error::other(err.clone()))?;
        states.push(mcp);
    }
    if states.iter().all(|s| *s == Status::Missing) {
        Ok(Status::Missing)
    } else if states.iter().all(|s| *s == Status::Registered) {
        Ok(Status::Registered)
    } else {
        Ok(Status::NeedsRepair)
    }
}

pub fn status(agent: &str) -> io::Result<Status> {
    aggregate_status(&detailed_status(agent))
}

fn install_notify(agent: &str) -> io::Result<()> {
    let paths = paths(agent)?;
    if is_hook_agent(agent) {
        return update_hook_file(&paths.registration, agent, false);
    }
    write_assets(&paths, agent)?;
    register_link(
        &paths.registration,
        &paths.assets.join("extension"),
        &paths.managed,
    )
}

fn uninstall_notify(agent: &str) -> io::Result<()> {
    let paths = paths(agent)?;
    if is_hook_agent(agent) {
        update_hook_file(&paths.registration, agent, true)?;
        // Grok's file is Kumu-owned (`~/.grok/hooks/kumu-notifications.json`):
        // remove it once empty instead of leaving `{}` behind.
        if agent == "grok"
            && let Ok(text) = std::fs::read_to_string(&paths.registration)
            && serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.as_object().map(|obj| obj.is_empty()))
                .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&paths.registration);
        }
        return Ok(());
    }
    remove_link(
        &paths.registration,
        &paths.assets.join("extension"),
        &paths.managed,
    )
}

pub fn install_detailed(agent: &str) -> ComponentOutcome {
    let notify = install_notify(agent).map_err(|e| e.to_string());
    let mcp = supports_mcp(agent).then(|| install_mcp(agent).map_err(|e| e.to_string()));
    let skills = install_skills(agent).map_err(|e| e.to_string());
    ComponentOutcome {
        notify,
        mcp,
        skills,
    }
}

pub fn install(agent: &str) -> io::Result<()> {
    install_detailed(agent).into_io_result()
}

/// Copy this build's extension files atomically into Kumu's managed directory.
fn write_assets(paths: &Paths, agent: &str) -> io::Result<()> {
    for (relative, contents) in asset_files(agent) {
        let file = paths.assets.join(relative);
        let parent = file.parent().expect("asset directory");
        std::fs::create_dir_all(parent)?;
        if std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(io::Error::other(format!(
                "托管扩展文件不应为符号链接：{}",
                file.display()
            )));
        }
        if std::fs::read_to_string(&file).ok().as_deref() == Some(contents.as_str()) {
            continue;
        }
        let temporary = temporary_path(&file);
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)?;
        let result = (|| {
            out.write_all(contents.as_bytes())?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&temporary, &file)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
    }
    Ok(())
}

/// Bring Kumu's managed registrations up to date at startup.
///
/// Only components that are already installed are refreshed: native extensions
/// (OpenCode, Pi, OMP) whose link already points at Kumu, `~/.kumu/bin/kumu-mcp`
/// when present, `~/.kumu/skills` when present, and Claude Code / Antigravity
/// hooks when their hook entry is already registered and stale. Never installs
/// missing components silently.
pub fn refresh_installed() {
    mcp::refresh_mcp_wrapper();
    skills::refresh_skills();
    refresh_native();
    for agent in ["claude", "agy"] {
        if matches!(notify_status(agent), Ok(Status::NeedsRepair))
            && let Err(error) = install_notify(agent)
        {
            eprintln!("[agent] 刷新 {agent} hooks 失败：{error}");
        }
    }
}

/// Bring Kumu's own extension files up to date at startup, for an agent whose
/// registration already points at them.
pub fn refresh_native() {
    for agent in ["opencode", "pi", "omp"] {
        let Ok(paths) = paths(agent) else {
            continue;
        };
        if registration_is_ours(&paths)
            && let Err(error) = refresh_registered(&paths, agent)
        {
            // Startup must not die over this; the settings row still reports
            // NeedsRepair, and this line says why.
            eprintln!("[agent] 刷新 {agent} 扩展失败：{error}");
        }
    }
}

/// Whether the registration is Kumu's own link (current or from an older
/// layout). Anything else — a user's file, another tool's symlink, no entry at
/// all — must be left alone by a background refresh.
fn registration_is_ours(paths: &Paths) -> bool {
    std::fs::read_link(&paths.registration).is_ok_and(|current| {
        current == paths.assets.join("extension") || is_under_managed(&current, &paths.managed)
    })
}

/// Rewrite the managed files and point the link at this build's extension.
fn refresh_registered(paths: &Paths, agent: &str) -> io::Result<()> {
    write_assets(paths, agent)?;
    register_link(
        &paths.registration,
        &paths.assets.join("extension"),
        &paths.managed,
    )
}

pub fn uninstall_detailed(agent: &str, force_shared: bool) -> ComponentOutcome {
    let notify = uninstall_notify(agent).map_err(|e| e.to_string());
    let mcp = supports_mcp(agent).then(|| uninstall_mcp(agent).map_err(|e| e.to_string()));
    let skills = uninstall_skills(agent, force_shared).map_err(|e| e.to_string());
    ComponentOutcome {
        notify,
        mcp,
        skills,
    }
}

pub fn uninstall(agent: &str) -> io::Result<()> {
    uninstall_detailed(agent, false).into_io_result()
}

fn register_link(entry: &Path, target: &Path, managed: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(entry) {
        Ok(_) if std::fs::read_link(entry).ok().as_deref() == Some(target) => return Ok(()),
        Ok(_) => {
            let previous = std::fs::read_link(entry).ok();
            if !previous
                .as_deref()
                .is_some_and(|path| is_under_managed(path, managed))
            {
                return Err(io::Error::other(format!(
                    "接入路径已有其他文件，未覆盖：{}",
                    entry.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(entry.parent().expect("registration directory"))?;
    let temporary = temporary_path(entry);
    symlink(target, &temporary)?;
    if let Err(error) = std::fs::rename(&temporary, entry) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn remove_link(entry: &Path, target: &Path, managed: &Path) -> io::Result<()> {
    match std::fs::read_link(entry) {
        Ok(current) if current == target => std::fs::remove_file(entry),
        Ok(current) if is_under_managed(&current, managed) => std::fs::remove_file(entry),
        Ok(_) => Err(io::Error::other(format!(
            "接入路径属于其他链接，未删除：{}",
            entry.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::cli::{
        notification_message, notification_status, session_id, session_sequence, status_sequence,
        title_sequence,
    };
    use super::hooks::{MARKER, agy_hook, edit_cursor_hooks, edit_hooks};
    use super::*;
    use crate::agent::status::{AgentState, StatusSignal};

    #[test]
    fn a_conversation_id_travels_as_a_silent_osc() {
        assert_eq!(
            session_sequence("codex", "abc-1", None).as_deref(),
            Some("\x1b]777;notify;codex;kumu-session:abc-1\x07")
        );
        assert_eq!(
            session_sequence("codex", "abc-1", Some("tok99")).as_deref(),
            Some("\x1b]777;notify;codex;kumu-session:tok99:abc-1\x07")
        );
        assert!(session_sequence("codex", "a;b", None).is_none());
        assert!(session_sequence("codex", "a\x07b", None).is_none());
        assert!(session_sequence("codex", "--yolo", Some("tok99")).is_none());
    }

    #[test]
    fn statuses_and_prompt_titles_are_silent_oscs() {
        assert_eq!(
            status_sequence("codex", StatusSignal::Active(AgentState::Waiting)),
            "\x1b]777;notify;codex;kumu-status:waiting\x07"
        );
        assert_eq!(
            status_sequence("claude", StatusSignal::Resume),
            "\x1b]777;notify;claude;kumu-status:resume\x07"
        );
        assert_eq!(
            title_sequence("claude", "Fix sidebar status\nmore details", true).as_deref(),
            Some("\x1b]777;notify;claude;kumu-title:prompt:Fix sidebar status\x07")
        );
        assert_eq!(title_sequence("claude", "/compact", true), None);
        assert_eq!(
            notification_status("claude", "start", None),
            Some(StatusSignal::Active(AgentState::Running))
        );
        assert_eq!(
            notification_status("claude", "resume", None),
            Some(StatusSignal::Resume)
        );
        assert_eq!(notification_message("claude", "start", None), None);
        assert_eq!(notification_message("claude", "resume", None), None);
        assert_eq!(
            notification_status("claude", "complete", None),
            Some(StatusSignal::Active(AgentState::Completed))
        );
        assert_eq!(
            notification_status("claude", "permission", None),
            Some(StatusSignal::Active(AgentState::Waiting))
        );
        assert_eq!(notification_status("claude", "session", None), None);
        assert_eq!(
            notification_status(
                "agy",
                "complete",
                Some(&json!({"terminationReason": "ERROR"}))
            ),
            Some(StatusSignal::Active(AgentState::Failed))
        );
        assert_eq!(
            notification_status(
                "agy",
                "complete",
                Some(&json!({"terminationReason": "USER_CANCELED"}))
            ),
            Some(StatusSignal::Idle)
        );
    }

    #[test]
    fn removal_keeps_unrelated_user_empty_hook_arrays_and_rejects_parent_traversal() {
        let with_empty =
            edit_hooks(r#"{"hooks":{"BeforeTool":[]}}"#, "claude", false).expect("install");
        let after = edit_hooks(&with_empty, "claude", true).expect("remove");
        let parsed: Value = serde_json::from_str(&after).expect("json");
        assert_eq!(parsed["hooks"]["BeforeTool"], json!([]));

        let managed = Path::new("/Users/me/.kumu/agent-integration");
        assert!(is_under_managed(
            Path::new("/Users/me/.kumu/agent-integration/pi/extension"),
            managed
        ));
        assert!(!is_under_managed(
            Path::new("/Users/me/.kumu/agent-integration/../../other"),
            managed
        ));
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

    #[test]
    fn agy_hooks_live_under_their_own_named_entry_and_preserve_user_hooks() {
        let original = r#"{
  // keep my comment
  "lint-checker": {
    "PostToolUse": [
      {
        "matcher": "run_command",
        "hooks": [{"type": "command", "command": "./lint.sh"}]
      }
    ]
  }
}
"#;
        let registered = edit_hooks(original, "agy", false).expect("register");
        assert!(registered.contains("// keep my comment"), "{registered}");
        assert!(registered.contains("\"lint-checker\""), "{registered}");
        assert!(
            registered.contains("\"kumu-notifications\""),
            "{registered}"
        );
        assert!(registered.contains("\"SessionStart\""), "{registered}");
        assert!(registered.contains("\"PreInvocation\""), "{registered}");
        assert!(registered.contains("\"Stop\""), "{registered}");
        assert!(registered.contains("printf '{}'"), "{registered}");
        assert_eq!(
            registered,
            edit_hooks(&registered, "agy", false).expect("repair"),
            "repair must be idempotent"
        );
        let parsed = CstRootNode::parse(&registered, &ParseOptions::default())
            .expect("valid jsonc")
            .to_serde_value()
            .expect("value");
        assert_eq!(agy_hooks_status(&parsed), Status::Registered);

        let removed = edit_hooks(&registered, "agy", true).expect("remove");
        assert!(!removed.contains(MARKER), "{removed}");
        assert!(!removed.contains("\"kumu-notifications\""), "{removed}");
        assert!(removed.contains("\"lint-checker\""), "{removed}");
        assert!(removed.contains("// keep my comment"), "{removed}");
        let parsed_removed = CstRootNode::parse(&removed, &ParseOptions::default())
            .expect("valid jsonc")
            .to_serde_value()
            .expect("value");
        assert_eq!(agy_hooks_status(&parsed_removed), Status::Missing);
    }

    #[test]
    fn agy_status_and_repair_handle_disabled_and_stale_hooks() {
        let disabled = json!({
            "kumu-notifications": {
                "enabled": false,
                "SessionStart": [agy_hook("session")],
                "Stop": [agy_hook("complete")],
            }
        });
        assert_eq!(agy_hooks_status(&disabled), Status::NeedsRepair);

        let repaired = edit_hooks(&disabled.to_string(), "agy", false).expect("repair");
        let parsed = CstRootNode::parse(&repaired, &ParseOptions::default())
            .expect("valid json")
            .to_serde_value()
            .expect("value");
        assert_eq!(agy_hooks_status(&parsed), Status::Registered);
        assert!(parsed["kumu-notifications"].get("enabled").is_none());
    }

    #[test]
    fn agy_notification_messages_respect_session_cancel_and_error() {
        assert_eq!(notification_message("agy", "session", None), None);
        assert_eq!(
            notification_message("agy", "complete", None),
            Some("本轮任务已结束")
        );
        assert_eq!(
            notification_message(
                "agy",
                "complete",
                Some(&json!({"terminationReason": "NO_TOOL_CALL"}))
            ),
            Some("本轮任务已结束")
        );
        assert_eq!(
            notification_message(
                "agy",
                "complete",
                Some(&json!({"terminationReason": "USER_CANCELED"}))
            ),
            None
        );
        assert_eq!(
            notification_message(
                "agy",
                "complete",
                Some(&json!({"terminationReason": "ERROR"}))
            ),
            Some("任务执行失败")
        );
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

    fn native_paths(dir: &Path) -> Paths {
        let managed = dir.join("managed");
        Paths {
            assets: managed.join("opencode"),
            managed,
            registration: dir.join("plugins/kumu-notifications"),
        }
    }

    /// Startup refresh must bring a registered native extension up to this
    /// build, repoint an older Kumu link, and leave anything that is not ours
    /// exactly as it is. Hook-based agents need none of this: their handler
    /// runs the current binary.
    #[test]
    fn startup_refresh_updates_our_extension_and_leaves_foreign_entries_alone() {
        let dir = fixture_dir();
        let paths = native_paths(&dir);
        let extension = paths.assets.join("extension");
        std::fs::create_dir_all(&extension).expect("managed extension");
        std::fs::create_dir_all(paths.registration.parent().expect("plugin dir")).expect("dir");
        std::fs::write(paths.assets.join("notify.js"), "old plugin").expect("stale asset");
        symlink(&extension, &paths.registration).expect("registered link");
        assert!(registration_is_ours(&paths));

        refresh_registered(&paths, "opencode").expect("refresh");
        for (relative, contents) in asset_files("opencode") {
            assert_eq!(
                std::fs::read_to_string(paths.assets.join(relative)).expect("asset"),
                contents,
                "{relative}"
            );
        }
        assert_eq!(
            std::fs::read_link(&paths.registration).expect("link"),
            extension
        );

        // A link from an older Kumu layout is ours and gets re-pointed.
        let old = paths.managed.join("old/extension");
        std::fs::create_dir_all(&old).expect("old target");
        std::fs::remove_file(&paths.registration).expect("replace link");
        symlink(&old, &paths.registration).expect("old link");
        assert!(registration_is_ours(&paths));
        refresh_registered(&paths, "opencode").expect("repair");
        assert_eq!(
            std::fs::read_link(&paths.registration).expect("repaired link"),
            extension
        );

        // A file and another tool's link are not ours and stay untouched.
        std::fs::remove_file(&paths.registration).expect("remove link");
        std::fs::write(&paths.registration, "another plugin").expect("foreign file");
        assert!(!registration_is_ours(&paths));
        assert_eq!(
            std::fs::read_to_string(&paths.registration).expect("foreign untouched"),
            "another plugin"
        );
        std::fs::remove_file(&paths.registration).expect("remove file");
        symlink(dir.join("elsewhere/extension"), &paths.registration).expect("foreign link");
        assert!(!registration_is_ours(&paths));

        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn aggregate_status_combines_notify_mcp_and_skills_without_early_return() {
        let all_missing = IntegrationDetail {
            notify: Ok(Status::Missing),
            mcp: McpState::Supported(Ok(Status::Missing)),
            skills: Ok(Status::Missing),
        };
        assert_eq!(aggregate_status(&all_missing).unwrap(), Status::Missing);

        let all_registered = IntegrationDetail {
            notify: Ok(Status::Registered),
            mcp: McpState::ExtensionPartial,
            skills: Ok(Status::Registered),
        };
        assert_eq!(
            aggregate_status(&all_registered).unwrap(),
            Status::Registered
        );

        // Skills installed while notify/mcp missing must report NeedsRepair, not Missing.
        let skills_only = IntegrationDetail {
            notify: Ok(Status::Missing),
            mcp: McpState::Supported(Ok(Status::Missing)),
            skills: Ok(Status::Registered),
        };
        assert_eq!(aggregate_status(&skills_only).unwrap(), Status::NeedsRepair);

        // Legacy user with notify+mcp but no skills reports NeedsRepair.
        let missing_skills = IntegrationDetail {
            notify: Ok(Status::Registered),
            mcp: McpState::Supported(Ok(Status::Registered)),
            skills: Ok(Status::Missing),
        };
        assert_eq!(
            aggregate_status(&missing_skills).unwrap(),
            Status::NeedsRepair
        );

        let errored = IntegrationDetail {
            notify: Ok(Status::Registered),
            mcp: McpState::Supported(Err("bad mcp".into())),
            skills: Ok(Status::Registered),
        };
        assert!(aggregate_status(&errored).is_err());
    }
}
