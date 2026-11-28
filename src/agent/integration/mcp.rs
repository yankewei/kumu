//! Automatic MCP server registration for supported coding agents.
//!
//! Writes a stable wrapper executable at `~/.kumu/bin/kumu-mcp` that delegates
//! to `"$KUMU_APP_BINARY" mcp` (falling back to the last launched Kumu binary),
//! then registers it non-destructively into each agent's MCP configuration file:
//! - Claude Code: `~/.claude.json` (`mcpServers.kumu`)
//! - Cursor CLI:  `~/.cursor/mcp.json` (`mcpServers.kumu`)
//! - Antigravity: `~/.gemini/antigravity/mcp_config.json` (`mcpServers.kumu`)
//! - OpenCode:    `~/.config/opencode/opencode.json` (`mcp.kumu`)
//! - Grok Build:  `~/.grok/mcp.json` (`mcpServers.kumu`)
//! - Codex:       `~/.codex/config.toml` (`[mcp_servers.kumu]`)

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde_json::{Value, json};

use super::hooks::temporary_path;
use super::{Status, non_empty_env_path};

const MCP_SERVER_KEY: &str = "kumu";

/// Agents that support external MCP configuration files managed by Kumu.
pub const MCP_AGENTS: [&str; 6] = ["claude", "codex", "cursor", "agy", "opencode", "grok"];

pub fn mcp_agents() -> &'static [&'static str] {
    &MCP_AGENTS
}

pub fn supports_mcp(agent: &str) -> bool {
    MCP_AGENTS.contains(&agent)
}

fn home_dir() -> io::Result<PathBuf> {
    crate::db::db_path()
        .parent()
        .and_then(Path::parent)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("无法确定用户主目录（HOME 未设置）"))
}

/// Stable executable wrapper path (`~/.kumu/bin/kumu-mcp`).
pub fn mcp_wrapper_path() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".kumu/bin/kumu-mcp"))
}

/// Ensure `~/.kumu/bin/kumu-mcp` exists, is executable, and falls back to the
/// current Kumu binary path when invoked outside a Kumu terminal pane.
pub fn ensure_mcp_wrapper() -> io::Result<PathBuf> {
    let wrapper = mcp_wrapper_path()?;
    let fallback_exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kumu".to_string());
    let escaped = fallback_exe.replace('\'', "'\\''");
    let script = format!(
        "#!/bin/sh\n\
         # Managed by Kumu. Runs Kumu's stdio MCP server.\n\
         if [ -n \"${{KUMU_APP_BINARY:-}}\" ] && [ -x \"$KUMU_APP_BINARY\" ]; then\n\
           exec \"$KUMU_APP_BINARY\" mcp \"$@\"\n\
         fi\n\
         exec '{escaped}' mcp \"$@\"\n"
    );
    if std::fs::read_to_string(&wrapper).ok().as_deref() == Some(script.as_str()) {
        return Ok(wrapper);
    }
    if let Some(parent) = wrapper.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temporary_path(&wrapper);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp)?;
    let write_res = (|| {
        file.write_all(script.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &wrapper)
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_res?;
    Ok(wrapper)
}

fn config_path(agent: &str) -> io::Result<PathBuf> {
    let home = home_dir()?;
    let path = match agent {
        "claude" => home.join(".claude.json"),
        "codex" => non_empty_env_path("CODEX_HOME")
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml"),
        "cursor" => non_empty_env_path("CURSOR_CONFIG_DIR")
            .or_else(|| non_empty_env_path("XDG_CONFIG_HOME").map(|p| p.join("cursor")))
            .unwrap_or_else(|| home.join(".cursor"))
            .join("mcp.json"),
        "agy" => home.join(".gemini/antigravity/mcp_config.json"),
        "opencode" => non_empty_env_path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode/opencode.json"),
        "grok" => home.join(".grok/mcp.json"),
        _ => return Err(io::Error::other("不支持该 Agent 的 MCP 自动注册")),
    };
    Ok(path)
}

pub fn mcp_status(agent: &str) -> io::Result<Status> {
    if !supports_mcp(agent) {
        return Ok(Status::Missing);
    }
    let wrapper = mcp_wrapper_path()?;
    let wrapper_str = wrapper.to_string_lossy();
    let path = config_path(agent)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Status::Missing),
        Err(err) => return Err(err),
    };
    if agent == "codex" {
        return Ok(codex_toml_status(&text, &wrapper_str));
    }
    let root = CstRootNode::parse(&text, &ParseOptions::default()).map_err(io::Error::other)?;
    let value = root
        .to_serde_value()
        .ok_or_else(|| io::Error::other("无效的 MCP 配置文件"))?;
    Ok(json_mcp_status(&value, agent, &wrapper_str))
}

pub fn install_mcp(agent: &str) -> io::Result<()> {
    if !supports_mcp(agent) {
        return Ok(());
    }
    let wrapper = ensure_mcp_wrapper()?;
    let wrapper_str = wrapper.to_string_lossy();
    let path = config_path(agent)?;
    update_mcp_file(&path, agent, &wrapper_str, false)
}

pub fn uninstall_mcp(agent: &str) -> io::Result<()> {
    if !supports_mcp(agent) {
        return Ok(());
    }
    let wrapper = mcp_wrapper_path()?;
    let wrapper_str = wrapper.to_string_lossy();
    let path = config_path(agent)?;
    if !path.exists() {
        return Ok(());
    }
    update_mcp_file(&path, agent, &wrapper_str, true)
}

/// Refresh the wrapper script at startup when it already exists so moving or
/// upgrading Kumu.app keeps all agent MCP registrations working automatically.
pub fn refresh_mcp_wrapper() {
    if let Ok(path) = mcp_wrapper_path()
        && path.exists()
        && let Err(error) = ensure_mcp_wrapper()
    {
        eprintln!("[agent] 刷新 kumu-mcp 包装脚本失败：{error}");
    }
}

fn update_mcp_file(path: &Path, agent: &str, wrapper: &str, remove: bool) -> io::Result<()> {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if remove {
                return Ok(());
            }
            if agent == "codex" {
                String::new()
            } else {
                "{}\n".to_string()
            }
        }
        Err(err) => return Err(err),
    };

    let updated = if agent == "codex" {
        edit_codex_toml(&original, wrapper, remove)
    } else {
        edit_json_mcp(&original, agent, wrapper, remove)?
    };

    if updated == original && path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&target)
        .ok()
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o777
        })
        .unwrap_or(0o600);
    let tmp = temporary_path(&target);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let res = (|| {
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn mcp_parent_key(agent: &str) -> &'static str {
    if agent == "opencode" {
        "mcp"
    } else {
        "mcpServers"
    }
}

fn mcp_entry_value(agent: &str, wrapper: &str) -> Value {
    match agent {
        "opencode" => json!({
            "type": "local",
            "command": [wrapper],
            "enabled": true
        }),
        "claude" => json!({
            "type": "stdio",
            "command": wrapper,
            "args": []
        }),
        _ => json!({
            "command": wrapper,
            "args": []
        }),
    }
}

fn mcp_entry_cst(agent: &str, wrapper: &str) -> CstInputValue {
    match agent {
        "opencode" => CstInputValue::Object(vec![
            ("type".into(), CstInputValue::String("local".into())),
            (
                "command".into(),
                CstInputValue::Array(vec![CstInputValue::String(wrapper.into())]),
            ),
            ("enabled".into(), CstInputValue::Bool(true)),
        ]),
        "claude" => CstInputValue::Object(vec![
            ("type".into(), CstInputValue::String("stdio".into())),
            ("command".into(), CstInputValue::String(wrapper.into())),
            ("args".into(), CstInputValue::Array(Vec::new())),
        ]),
        _ => CstInputValue::Object(vec![
            ("command".into(), CstInputValue::String(wrapper.into())),
            ("args".into(), CstInputValue::Array(Vec::new())),
        ]),
    }
}

fn json_mcp_status(root: &Value, agent: &str, wrapper: &str) -> Status {
    let parent = mcp_parent_key(agent);
    let Some(entry) = root.get(parent).and_then(|obj| obj.get(MCP_SERVER_KEY)) else {
        return Status::Missing;
    };
    let expected = mcp_entry_value(agent, wrapper);
    if *entry == expected {
        Status::Registered
    } else {
        Status::NeedsRepair
    }
}

fn edit_json_mcp(text: &str, agent: &str, wrapper: &str, remove: bool) -> io::Result<String> {
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(io::Error::other)?;
    let object = root
        .object_value()
        .ok_or_else(|| io::Error::other("MCP 配置必须是 JSON 对象，未覆盖原文件"))?;
    let parent_key = mcp_parent_key(agent);

    if remove {
        let Some(servers) = object.object_value(parent_key) else {
            return Ok(text.to_string());
        };
        let Some(prop) = servers.get(MCP_SERVER_KEY) else {
            return Ok(text.to_string());
        };
        prop.remove();
        if servers.properties().is_empty()
            && let Some(parent_prop) = object.get(parent_key)
        {
            parent_prop.remove();
        }
        return Ok(root.to_string());
    }

    let servers = object
        .object_value_or_create(parent_key)
        .ok_or_else(|| io::Error::other(format!("{parent_key} 必须是对象，未覆盖原文件")))?;
    let expected = mcp_entry_cst(agent, wrapper);
    if let Some(prop) = servers.get(MCP_SERVER_KEY) {
        prop.set_value(expected);
    } else {
        servers.append(MCP_SERVER_KEY, expected);
    }
    Ok(root.to_string())
}

fn codex_toml_status(text: &str, wrapper: &str) -> Status {
    let mut in_section = false;
    let mut found_section = false;
    let mut command_matches = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == "[mcp_servers.kumu]";
            if in_section {
                found_section = true;
            }
            continue;
        }
        if in_section
            && let Some(rest) = trimmed.strip_prefix("command")
            && let Some(val) = rest.trim_start().strip_prefix('=')
        {
            let cleaned = val.trim().trim_matches('"').trim_matches('\'');
            if cleaned == wrapper {
                command_matches = true;
            }
        }
    }
    if !found_section {
        Status::Missing
    } else if command_matches {
        Status::Registered
    } else {
        Status::NeedsRepair
    }
}

fn edit_codex_toml(text: &str, wrapper: &str, remove: bool) -> String {
    // Strip any existing `[mcp_servers.kumu]` section up to the next `[...]` header.
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed == "[mcp_servers.kumu]" {
                skipping = true;
                continue;
            }
            skipping = false;
        }
        if !skipping {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }

    if remove {
        if kept.is_empty() {
            return String::new();
        }
        let mut out = kept.join("\n");
        out.push('\n');
        return out;
    }

    let escaped = wrapper.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "[mcp_servers.kumu]\ncommand = \"{escaped}\"\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_mcp_edit_preserves_comments_and_other_servers() {
        let original = r#"{
  // User comment
  "mcpServers": {
    "other": { "command": "npx", "args": ["-y", "other"] }
  }
}
"#;
        let wrapper = "/Users/me/.kumu/bin/kumu-mcp";
        let installed = edit_json_mcp(original, "claude", wrapper, false).expect("install");
        assert!(installed.contains("// User comment"));
        assert!(installed.contains("\"other\""));
        assert!(installed.contains(wrapper));

        let parsed = CstRootNode::parse(&installed, &ParseOptions::default())
            .expect("jsonc")
            .to_serde_value()
            .expect("value");
        assert_eq!(json_mcp_status(&parsed, "claude", wrapper), Status::Registered);

        let removed = edit_json_mcp(&installed, "claude", wrapper, true).expect("remove");
        assert!(removed.contains("\"other\""));
        assert!(!removed.contains(wrapper));
    }

    #[test]
    fn codex_toml_edit_round_trips_cleanly() {
        let original = "# keep me\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"other\"\n";
        let wrapper = "/Users/me/.kumu/bin/kumu-mcp";
        let installed = edit_codex_toml(original, wrapper, false);
        assert_eq!(codex_toml_status(&installed, wrapper), Status::Registered);
        assert!(installed.contains("# keep me"));
        assert!(installed.contains("[mcp_servers.other]"));

        let removed = edit_codex_toml(&installed, wrapper, true);
        assert_eq!(codex_toml_status(&removed, wrapper), Status::Missing);
        assert!(removed.contains("[mcp_servers.other]"));
        assert!(!removed.contains("[mcp_servers.kumu]"));
    }
}
