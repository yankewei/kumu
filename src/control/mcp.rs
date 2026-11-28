//! Dual-era Model Context Protocol (MCP) stdio server for Kumu.
//!
//! Compliant with MCP `2026-07-28` (stateless core, no required handshake,
//! `ToolAnnotations`, `outputSchema` + `structuredContent`, no JSON-RPC batching)
//! while remaining backward-compatible with legacy handshake clients
//! (`2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`).
//!
//! Subcommands:
//! - `kumu mcp` / `kumu mcp serve`: run the NDJSON stdio MCP server.
//! - `kumu mcp install [agent|all]`: register Kumu's MCP server in agent configs.
//! - `kumu mcp uninstall [agent|all]`: remove Kumu's MCP server from agent configs.
//! - `kumu mcp status [--json]`: inspect MCP registration status across agents.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::cli::{call, caller_pane, pane_target, print_json, runtime_error, usage_error};
use super::cli_artifact::{
    artifact_from_path, artifact_json, control_pane, notify_app, resolve_root,
};
use super::protocol::{PaneSummary, Request, ScrollDirection};
use super::{MAX_HISTORY, MAX_SCROLL_LINES};
use crate::artifacts::{self, ArtifactKind, PublishInput, PublishOptions, SessionInfo};

/// Latest MCP protocol version supported by Kumu.
pub const LATEST_PROTOCOL_VERSION: &str = "2026-07-28";

/// Protocol versions negotiated during legacy `initialize` handshakes.
const SUPPORTED_VERSIONS: [&str; 5] = [
    LATEST_PROTOCOL_VERSION,
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

const JSON_SCHEMA_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

const MCP_USAGE: &str = "\
usage:
  kumu mcp [serve]
  kumu mcp install [AGENT|all]
  kumu mcp uninstall [AGENT|all]
  kumu mcp status [--json]

Runs Kumu's stdio Model Context Protocol (MCP) server, or manages its
registration in supported coding agents (claude, codex, cursor, agy, opencode, grok).";

pub(super) fn mcp_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("serve") => serve_stdio(),
        Some("install") => manage_registration(&args[1..], false),
        Some("uninstall") | Some("remove") => manage_registration(&args[1..], true),
        Some("status") => status_cli(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{MCP_USAGE}");
            0
        }
        Some(other) => usage_error(&format!("unknown mcp subcommand {other}")),
    }
}

fn manage_registration(args: &[String], remove: bool) -> i32 {
    let target = match args {
        [] => "all",
        [only] if only == "-h" || only == "--help" => {
            println!("{MCP_USAGE}");
            return 0;
        }
        [only] => only.as_str(),
        _ => return usage_error("expected at most one agent id or 'all'"),
    };
    let agents: Vec<&'static str> = if target == "all" {
        crate::agent::integration::mcp_agents().to_vec()
    } else {
        let Some(id) = crate::agent::id_for(target) else {
            return usage_error(&format!("unknown agent '{target}'"));
        };
        if !crate::agent::integration::supports_mcp(id) {
            return runtime_error(&format!(
                "{id} uses Kumu's native extension instead of external MCP config"
            ));
        }
        vec![id]
    };

    let mut failed = false;
    for agent in agents {
        let result = if remove {
            crate::agent::integration::uninstall_mcp(agent)
        } else {
            crate::agent::integration::install_mcp(agent)
        };
        match result {
            Ok(()) => {
                let verb = if remove { "removed" } else { "installed" };
                println!("{agent}: {verb}");
            }
            Err(error) => {
                eprintln!("{agent}: {error}");
                failed = true;
            }
        }
    }
    if failed { 1 } else { 0 }
}

fn status_cli(args: &[String]) -> i32 {
    let mut json_out = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json_out = true,
            "-h" | "--help" => {
                println!("{MCP_USAGE}");
                return 0;
            }
            other => return usage_error(&format!("unknown option {other}")),
        }
    }
    let wrapper = crate::agent::integration::mcp_wrapper_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut rows = Vec::new();
    for &agent in crate::agent::integration::mcp_agents() {
        let state = match crate::agent::integration::mcp_status(agent) {
            Ok(crate::agent::integration::Status::Registered) => "registered".to_string(),
            Ok(crate::agent::integration::Status::NeedsRepair) => "needs_repair".to_string(),
            Ok(crate::agent::integration::Status::Missing) => "missing".to_string(),
            Err(err) => format!("error: {err}"),
        };
        rows.push(json!({ "agent": agent, "status": state }));
    }
    if json_out {
        return print_json(&json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "wrapper": wrapper,
            "agents": rows,
        }));
    }
    println!("wrapper: {wrapper}");
    for row in rows {
        println!(
            "{:<10} {}",
            row["agent"].as_str().unwrap_or(""),
            row["status"].as_str().unwrap_or("")
        );
    }
    println!("\nTip: run 'kumu integration status' to inspect notify, MCP, and skills together.");
    0
}

fn in_kumu_terminal() -> bool {
    std::env::var("KUMU_TERMINAL").as_deref() == Ok("1")
        || std::env::var_os("KUMU_PANE").is_some_and(|v| !v.is_empty())
}

/// Run the stdio NDJSON MCP loop until EOF on stdin.
fn serve_stdio() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(response) = handle_json_line(trimmed) {
                    let encoded = match serde_json::to_string(&response) {
                        Ok(text) => text,
                        Err(error) => {
                            eprintln!("[kumu-mcp] serialize error: {error}");
                            continue;
                        }
                    };
                    if writer.write_all(encoded.as_bytes()).is_err()
                        || writer.write_all(b"\n").is_err()
                        || writer.flush().is_err()
                    {
                        break;
                    }
                }
            }
            Err(error) => {
                eprintln!("[kumu-mcp] stdin read error: {error}");
                return 1;
            }
        }
    }
    0
}

/// Process one newline-delimited JSON-RPC 2.0 message.
///
/// Returns `None` for notifications (messages without `id`).
pub(crate) fn handle_json_line(raw: &str) -> Option<Value> {
    handle_json_line_with_env(raw, in_kumu_terminal())
}

fn handle_json_line_with_env(raw: &str, in_kumu: bool) -> Option<Value> {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            return Some(jsonrpc_error(
                Value::Null,
                -32700,
                &format!("Parse error: {error}"),
            ));
        }
    };

    // MCP 2025-06-18+ removed JSON-RPC batching.
    if parsed.is_array() {
        return Some(jsonrpc_error(
            Value::Null,
            -32600,
            "Invalid Request: JSON-RPC batching is not supported by MCP",
        ));
    }

    let Some(obj) = parsed.as_object() else {
        return Some(jsonrpc_error(
            Value::Null,
            -32600,
            "Invalid Request: expected a JSON object",
        ));
    };

    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        return Some(jsonrpc_error(
            id,
            -32600,
            "Invalid Request: jsonrpc must be \"2.0\"",
        ));
    }

    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // Response or malformed object without method; ignore if notification-shaped.
        return id.map(|id| jsonrpc_error(id, -32600, "Invalid Request: missing method"));
    };

    // Notifications (no `id`) never receive a JSON-RPC response.
    let id = id?;

    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "initialize" => Some(jsonrpc_ok(id, handle_initialize(&params, in_kumu))),
        "ping" => Some(jsonrpc_ok(id, json!({}))),
        "tools/list" => {
            let tools = if in_kumu {
                Value::Array(tool_definitions())
            } else {
                json!([])
            };
            Some(jsonrpc_ok(id, json!({ "tools": tools })))
        }
        "tools/call" => {
            if !in_kumu {
                return Some(jsonrpc_ok(
                    id,
                    tool_err("Kumu MCP tools are only available inside a Kumu terminal pane."),
                ));
            }
            match handle_tools_call(&params) {
                Ok(result) => Some(jsonrpc_ok(id, result)),
                Err((code, message)) => Some(jsonrpc_error(id, code, &message)),
            }
        }
        other => Some(jsonrpc_error(
            id,
            -32601,
            &format!("Method not found: {other}"),
        )),
    }
}

fn handle_initialize(params: &Value, in_kumu: bool) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let negotiated = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    let mut result = json!({
        "protocolVersion": negotiated,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": "kumu",
            "title": "Kumu",
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    if in_kumu {
        result["instructions"] = json!(
            "Kumu exposes tools to publish and view rich Markdown/HTML artifacts in the user's Kumu workspace, and to coordinate sibling agent terminal panes."
        );
    }
    result
}

fn jsonrpc_ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn tool_ok(summary: String, structured: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": summary }],
        "structuredContent": structured,
        "isError": false
    })
}

fn tool_err(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true
    })
}

fn handle_tools_call(params: &Value) -> Result<Value, (i64, String)> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err((-32602, "Invalid params: missing tool name".into()));
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return Err((-32602, "Invalid params: arguments must be an object".into()));
    }

    let result = match name {
        "artifact_publish" => call_artifact_publish(&args),
        "artifact_list" => call_artifact_list(&args),
        "artifact_open" => call_artifact_open(&args),
        "artifact_remove" => call_artifact_remove(&args),
        "pane_list" => call_pane_list(),
        "pane_read" => call_pane_read(&args),
        "pane_scroll" => call_pane_scroll(&args),
        "pane_split" => call_pane_split(&args),
        "pane_send" => call_pane_send(&args),
        "pane_wait" => call_pane_wait(&args),
        "pane_close" => call_pane_close(&args),
        other => return Err((-32602, format!("Unknown tool: {other}"))),
    };
    Ok(result)
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn call_artifact_publish(args: &Value) -> Value {
    let content = str_arg(args, "content");
    let file = str_arg(args, "file");
    if content.is_some() == file.is_some() {
        return tool_err("Provide exactly one of 'content' or 'file'.");
    }

    let title = str_arg(args, "title");
    let kind = match str_arg(args, "kind").as_deref() {
        None | Some("auto") => None,
        Some(raw) => match ArtifactKind::parse(raw) {
            Some(parsed) => Some(parsed),
            None => return tool_err("kind must be auto, markdown, html, or text"),
        },
    };
    let entry = str_arg(args, "entry");
    let replace = str_arg(args, "replace");
    let open_after = bool_arg(args, "open", true);
    let project = str_arg(args, "project").map(PathBuf::from);
    let tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let pane = caller_pane().and_then(control_pane);
    let explicit_root = pane
        .as_ref()
        .map(|p| p.project_path.as_str())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or(project);

    let project_root = match explicit_root {
        Some(root) => match artifacts::resolve_project_root(&root, Some(&root)) {
            Ok(root) => root,
            Err(error) => return tool_err(error.to_string()),
        },
        None => {
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(error) => return tool_err(format!("cannot read working directory: {error}")),
            };
            match artifacts::resolve_project_root_with_fallback(&cwd, None) {
                Ok((root, _)) => root,
                Err(error) => return tool_err(error.to_string()),
            }
        }
    };

    let session = match pane {
        Some(pane) if pane.agent.is_some() => {
            let PaneSummary {
                id,
                agent,
                agent_session_id,
                title,
                tab_name,
                cwd,
                ..
            } = pane;
            SessionInfo::new(
                Some(id),
                agent,
                agent_session_id,
                Some(title),
                (!tab_name.is_empty()).then_some(tab_name),
                (!cwd.is_empty()).then_some(cwd),
            )
        }
        _ => SessionInfo::new(
            None,
            None,
            None,
            None,
            None,
            std::env::current_dir()
                .ok()
                .map(|cwd| cwd.to_string_lossy().into_owned()),
        ),
    };

    let input = if let Some(text) = content {
        if text.is_empty() {
            return tool_err("content must not be empty");
        }
        PublishInput::Stdin(text.into_bytes())
    } else {
        let path = PathBuf::from(file.unwrap());
        if path.is_dir() {
            PublishInput::Directory(path)
        } else {
            PublishInput::File(path)
        }
    };

    let published = match artifacts::publish(PublishOptions {
        project_root: project_root.clone(),
        input,
        title,
        kind,
        entry,
        replace,
        session,
        tags,
        follow_symlinks: false,
    }) {
        Ok(artifact) => artifact,
        Err(error) => return tool_err(error.to_string()),
    };

    let _ = notify_app(
        Request::ArtifactPublished {
            from: caller_pane(),
            project_path: project_root.to_string_lossy().into_owned(),
            artifact: published.id().to_string(),
        },
        false,
    );

    let mut opened = false;
    if open_after {
        opened = notify_app(
            Request::ArtifactOpen {
                from: caller_pane(),
                project_path: project_root.to_string_lossy().into_owned(),
                artifact: published.id().to_string(),
            },
            true,
        )
        .is_ok();
    }

    let mut data = artifact_json(&published);
    if let Some(obj) = data.as_object_mut() {
        obj.insert("opened".into(), json!(opened));
    }
    let summary = format!(
        "Published artifact {} ({}, {}) at {}",
        published.id(),
        published.meta.title,
        published.meta.kind.as_str(),
        published.entry_path().display()
    );
    tool_ok(summary, data)
}

fn call_artifact_list(args: &Value) -> Value {
    let project = str_arg(args, "project").map(PathBuf::from);
    let session = str_arg(args, "session");
    let limit = u64_arg(args, "limit").map(|n| n as usize);

    let root = match resolve_root(project.as_deref()) {
        Ok(root) => root,
        Err(error) => return tool_err(error),
    };
    let mut found = artifacts::scan(&root);
    if let Some(session) = session {
        found.retain(|a| a.meta.session.key == session);
    }
    if let Some(limit) = limit {
        found.truncate(limit);
    }
    let items: Vec<Value> = found.iter().map(artifact_json).collect();
    let summary = if found.is_empty() {
        format!("No artifacts in {}", root.display())
    } else {
        found
            .iter()
            .map(|a| format!("{} [{}] {}", a.id(), a.meta.kind.as_str(), a.meta.title))
            .collect::<Vec<_>>()
            .join("\n")
    };
    tool_ok(
        summary,
        json!({
            "projectRoot": root,
            "artifacts": items,
        }),
    )
}

fn call_artifact_open(args: &Value) -> Value {
    let Some(target) = str_arg(args, "target") else {
        return tool_err("Missing required argument 'target' (artifact id or path).");
    };
    let project = str_arg(args, "project").map(PathBuf::from);
    let (root, id) = match artifact_from_path(&target) {
        Some((path_root, path_id)) if project.is_none() => (path_root, path_id),
        _ => {
            let root = match resolve_root(project.as_deref()) {
                Ok(root) => root,
                Err(error) => return tool_err(error),
            };
            match artifacts::find(&root, &target) {
                Some(artifact) => (root, artifact.id().to_string()),
                None => {
                    return tool_err(format!(
                        "Artifact '{target}' does not exist in {}",
                        root.display()
                    ));
                }
            }
        }
    };
    match notify_app(
        Request::ArtifactOpen {
            from: caller_pane(),
            project_path: root.to_string_lossy().into_owned(),
            artifact: id.clone(),
        },
        true,
    ) {
        Ok(_) => tool_ok(
            format!("Opened artifact {id} in Kumu"),
            json!({ "id": id, "opened": true }),
        ),
        Err(error) => tool_err(error),
    }
}

fn call_artifact_remove(args: &Value) -> Value {
    let Some(id) = str_arg(args, "id") else {
        return tool_err("Missing required argument 'id'.");
    };
    let project = str_arg(args, "project").map(PathBuf::from);
    let root = match resolve_root(project.as_deref()) {
        Ok(root) => root,
        Err(error) => return tool_err(error),
    };
    if artifacts::find(&root, &id).is_none() {
        return tool_err(format!(
            "Artifact '{id}' does not exist in {}",
            root.display()
        ));
    }
    match artifacts::move_to_trash(&root, &id) {
        Ok(trashed) => {
            let _ = notify_app(
                Request::ArtifactPublished {
                    from: caller_pane(),
                    project_path: root.to_string_lossy().into_owned(),
                    artifact: id.clone(),
                },
                false,
            );
            tool_ok(
                format!("Moved artifact {id} to Trash: {}", trashed.display()),
                json!({ "id": id, "trashedPath": trashed }),
            )
        }
        Err(error) => tool_err(error.to_string()),
    }
}

fn call_pane_list() -> Value {
    match call(&Request::Panes) {
        Ok(value) => {
            let summary = value
                .get("panes")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|p| {
                            format!(
                                "#{} [{}] {} ({})",
                                p["id"].as_u64().unwrap_or(0),
                                p["agent"].as_str().unwrap_or("shell"),
                                p["title"].as_str().unwrap_or(""),
                                p["projectName"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            tool_ok(summary, value)
        }
        Err(error) => tool_err(error),
    }
}

fn call_pane_read(args: &Value) -> Value {
    let target = pane_target(str_arg(args, "target"));
    let history = u64_arg(args, "history")
        .map(|n| (n as usize).min(MAX_HISTORY))
        .unwrap_or(0);
    let tail = u64_arg(args, "tail").map(|n| n as usize);
    match call(&Request::Read {
        pane: target,
        from: caller_pane(),
        history,
    }) {
        Ok(mut value) => {
            if let Some(tail_n) = tail
                && let Some(text) = value.get("text").and_then(Value::as_str)
            {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(tail_n);
                let trimmed = lines[start..].join("\n");
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("text".into(), json!(trimmed));
                }
            }
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            tool_ok(text, value)
        }
        Err(error) => tool_err(error),
    }
}

fn call_pane_scroll(args: &Value) -> Value {
    let direction = match str_arg(args, "direction").as_deref() {
        Some("up") => ScrollDirection::Up,
        Some("down") => ScrollDirection::Down,
        _ => return tool_err("direction must be 'up' or 'down'"),
    };
    let lines = u64_arg(args, "lines").map(|n| (n as u16).clamp(1, MAX_SCROLL_LINES));
    match call(&Request::Scroll {
        pane: pane_target(str_arg(args, "target")),
        from: caller_pane(),
        direction,
        lines,
        col: None,
        row: None,
    }) {
        Ok(value) => tool_ok(format!("Scrolled {}", direction.as_str()), value),
        Err(error) => tool_err(error),
    }
}

fn call_pane_split(args: &Value) -> Value {
    let Some(direction) = str_arg(args, "direction") else {
        return tool_err("Missing required argument 'direction' ('right' or 'down').");
    };
    let Some(agent) = str_arg(args, "agent") else {
        return tool_err("Missing required argument 'agent'.");
    };
    let prompt = str_arg(args, "prompt").filter(|p| !p.trim().is_empty());
    match call(&Request::Split {
        direction,
        agent,
        from: caller_pane(),
        prompt,
    }) {
        Ok(value) => {
            let id = value
                .pointer("/pane/id")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            tool_ok(format!("Opened agent pane #{id}"), value)
        }
        Err(error) => tool_err(error),
    }
}

fn call_pane_send(args: &Value) -> Value {
    let Some(target) = str_arg(args, "target") else {
        return tool_err("Missing required argument 'target'.");
    };
    let Some(text) = str_arg(args, "text") else {
        return tool_err("Missing required argument 'text'.");
    };
    let enter = bool_arg(args, "enter", true);
    match call(&Request::Send {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        text,
        enter,
    }) {
        Ok(value) => tool_ok("Sent text to agent pane".into(), value),
        Err(error) => tool_err(error),
    }
}

fn call_pane_wait(args: &Value) -> Value {
    let Some(target) = str_arg(args, "target") else {
        return tool_err("Missing required argument 'target'.");
    };
    let timeout_secs = u64_arg(args, "timeoutSecs").unwrap_or(300);
    let next = bool_arg(args, "next", false);
    match call(&Request::Wait {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        timeout_secs,
        next,
    }) {
        Ok(value) => {
            let state = value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            tool_ok(format!("Pane entered state: {state}"), value)
        }
        Err(error) => tool_err(error),
    }
}

fn call_pane_close(args: &Value) -> Value {
    let Some(target) = str_arg(args, "target") else {
        return tool_err("Missing required argument 'target'.");
    };
    let allow_self = bool_arg(args, "allowSelf", false);
    match call(&Request::Close {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        allow_self,
    }) {
        Ok(value) => tool_ok("Closed pane".into(), value),
        Err(error) => tool_err(error),
    }
}

/// All tool definitions with JSON Schema 2020-12 `inputSchema`, `outputSchema`,
/// and MCP `ToolAnnotations`.
pub(crate) fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "artifact_publish",
            "title": "Publish Artifact to Kumu",
            "description": "Publish a Markdown, HTML, or text artifact into the current project's Kumu Artifacts drawer and render it in-app. Pass either raw 'content' or a local 'file' (or directory) path. Automatically associates with the calling pane's agent session.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "content": { "type": "string", "description": "Raw document content (Markdown, HTML, or text) to publish directly without writing a temp file." },
                    "file": { "type": "string", "description": "Path to a local file or directory to snapshot and publish." },
                    "title": { "type": "string", "description": "Human-readable title shown in Kumu's Artifacts drawer." },
                    "kind": { "type": "string", "enum": ["auto", "markdown", "html", "text"], "default": "auto" },
                    "entry": { "type": "string", "description": "Relative entry file when publishing a directory (defaults to index.html / index.md)." },
                    "replace": { "type": "string", "description": "Existing artifact id to update in place while preserving its id and session." },
                    "open": { "type": "boolean", "default": true, "description": "Open the artifact reading sheet in Kumu immediately after publishing." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "project": { "type": "string", "description": "Optional project root override when running outside Kumu." }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["id", "title", "kind", "entry", "opened"],
                "properties": {
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "kind": { "type": "string" },
                    "entry": { "type": "string" },
                    "opened": { "type": "boolean" }
                }
            },
            "annotations": {
                "title": "Publish Artifact",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "artifact_list",
            "title": "List Project Artifacts",
            "description": "List artifacts stored in the current project's .kumu/artifacts directory, optionally filtered by session key or limited in count.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "session": { "type": "string", "description": "Optional session key filter." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of artifacts to return." },
                    "project": { "type": "string", "description": "Optional project root override." }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["projectRoot", "artifacts"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "artifacts": { "type": "array" }
                }
            },
            "annotations": {
                "title": "List Artifacts",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "artifact_open",
            "title": "Open Artifact in Kumu",
            "description": "Open an existing artifact (by id or path inside .kumu/artifacts) in Kumu's in-app reading sheet.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["target"],
                "properties": {
                    "target": { "type": "string", "description": "Artifact id or path inside .kumu/artifacts." },
                    "project": { "type": "string", "description": "Optional project root override." }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["id", "opened"],
                "properties": {
                    "id": { "type": "string" },
                    "opened": { "type": "boolean" }
                }
            },
            "annotations": {
                "title": "Open Artifact",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "artifact_remove",
            "title": "Remove Artifact to Trash",
            "description": "Move an artifact directory from .kumu/artifacts to the macOS Trash.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Artifact id to move to Trash." },
                    "project": { "type": "string", "description": "Optional project root override." }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["id", "trashedPath"],
                "properties": {
                    "id": { "type": "string" },
                    "trashedPath": { "type": "string" }
                }
            },
            "annotations": {
                "title": "Remove Artifact",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_list",
            "title": "List Kumu Panes",
            "description": "List all open terminal and agent panes in the current Kumu window.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["panes"],
                "properties": { "panes": { "type": "array" } }
            },
            "annotations": {
                "title": "List Panes",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_read",
            "title": "Read Terminal Pane Output",
            "description": "Read plain text from a sibling Kumu pane (id, left/right/up/down, focused, or self) plus optional scrollback history.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "target": { "type": "string", "description": "Pane id, direction (left/right/up/down), focused, or self." },
                    "history": { "type": "integer", "minimum": 0, "maximum": MAX_HISTORY, "default": 0 },
                    "tail": { "type": "integer", "minimum": 1, "description": "Keep only the last N lines." }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["pane", "text"],
                "properties": {
                    "pane": { "type": "object" },
                    "text": { "type": "string" }
                }
            },
            "annotations": {
                "title": "Read Pane",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_scroll",
            "title": "Scroll Terminal Pane",
            "description": "Scroll a target Kumu pane up or down.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["direction"],
                "properties": {
                    "direction": { "type": "string", "enum": ["up", "down"] },
                    "target": { "type": "string" },
                    "lines": { "type": "integer", "minimum": 1, "maximum": MAX_SCROLL_LINES }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["pane", "direction", "lines"],
                "properties": {
                    "pane": { "type": "object" },
                    "direction": { "type": "string" },
                    "lines": { "type": "integer" }
                }
            },
            "annotations": {
                "title": "Scroll Pane",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_split",
            "title": "Split New Agent Pane",
            "description": "Split a new pane (right or down) running the specified coding agent and optionally submit an initial prompt.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["direction", "agent"],
                "properties": {
                    "direction": { "type": "string", "enum": ["right", "down"] },
                    "agent": { "type": "string", "description": "Agent id (codex, claude, opencode, pi, omp, cursor, grok, agy)." },
                    "prompt": { "type": "string", "description": "Optional initial task prompt sent once the agent is ready." }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["pane"],
                "properties": { "pane": { "type": "object" } }
            },
            "annotations": {
                "title": "Split Agent Pane",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_send",
            "title": "Send Prompt to Agent Pane",
            "description": "Paste and submit text into a sibling agent pane (refuses plain shell panes).",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["target", "text"],
                "properties": {
                    "target": { "type": "string" },
                    "text": { "type": "string" },
                    "enter": { "type": "boolean", "default": true }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["pane", "submitted"],
                "properties": {
                    "pane": { "type": "object" },
                    "submitted": { "type": "boolean" }
                }
            },
            "annotations": {
                "title": "Send to Agent Pane",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_wait",
            "title": "Wait for Agent Pane State",
            "description": "Block until the target agent pane enters waiting, completed, failed, or exits.",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["target"],
                "properties": {
                    "target": { "type": "string" },
                    "timeoutSecs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 300 },
                    "next": { "type": "boolean", "default": false }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["pane", "state", "elapsedMs"],
                "properties": {
                    "pane": { "type": "object" },
                    "state": { "type": "string" },
                    "elapsedMs": { "type": "integer" }
                }
            },
            "annotations": {
                "title": "Wait for Pane",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "pane_close",
            "title": "Close Terminal Pane",
            "description": "Close a sibling terminal pane (refuses closing caller's own pane unless allowSelf is true).",
            "inputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["target"],
                "properties": {
                    "target": { "type": "string" },
                    "allowSelf": { "type": "boolean", "default": false }
                }
            },
            "outputSchema": {
                "$schema": JSON_SCHEMA_2020_12,
                "type": "object",
                "required": ["pane", "closed"],
                "properties": {
                    "pane": { "type": "integer" },
                    "closed": { "type": "boolean" }
                }
            },
            "annotations": {
                "title": "Close Pane",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_era_supports_stateless_calls_and_legacy_initialize() {
        // Modern 2026-07-28 client: calls tools/list directly without initialize.
        let list =
            handle_json_line_with_env(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, true)
                .expect("tools/list reply");
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert!(tools.len() >= 11);
        for tool in tools {
            assert!(tool.get("inputSchema").is_some(), "{tool}");
            assert!(tool.get("outputSchema").is_some(), "{tool}");
            assert!(tool.get("annotations").is_some(), "{tool}");
        }

        // Legacy client: negotiates 2025-11-25 via initialize, notification produces no reply.
        let init = handle_json_line_with_env(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            true,
        )
        .expect("init reply");
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert!(init["result"].get("instructions").is_some());
        assert!(
            handle_json_line_with_env(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                true
            )
            .is_none()
        );

        // Batching is rejected per MCP 2025-06-18+.
        let batch =
            handle_json_line_with_env(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, true)
                .expect("batch error");
        assert_eq!(batch["error"]["code"], -32600);
    }

    #[test]
    fn non_kumu_environment_silences_mcp_tools_and_instructions() {
        let init = handle_json_line_with_env(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}"#,
            false,
        )
        .expect("init reply");
        assert!(init["result"].get("instructions").is_none());

        let list =
            handle_json_line_with_env(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, false)
                .expect("tools/list reply");
        assert_eq!(list["result"]["tools"], json!([]));

        let call = handle_json_line_with_env(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"pane_list","arguments":{}}}"#,
            false,
        )
        .expect("tools/call reply");
        assert_eq!(call["result"]["isError"], true);
    }

    #[test]
    fn artifact_publish_and_list_round_trip_via_mcp() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-mcp-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let pub_req = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "artifact_publish",
                "arguments": {
                    "content": "# Hello MCP\n\nRendered in Kumu.",
                    "title": "MCP Demo",
                    "kind": "markdown",
                    "open": false,
                    "project": dir.to_string_lossy()
                }
            }
        });
        let pub_res = handle_json_line_with_env(&pub_req.to_string(), true).expect("publish reply");
        assert_eq!(pub_res["result"]["isError"], false);
        let id = pub_res["result"]["structuredContent"]["id"]
            .as_str()
            .expect("artifact id")
            .to_string();
        assert_eq!(pub_res["result"]["structuredContent"]["title"], "MCP Demo");

        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "artifact_list",
                "arguments": {
                    "project": dir.to_string_lossy()
                }
            }
        });
        let list_res = handle_json_line_with_env(&list_req.to_string(), true).expect("list reply");
        assert_eq!(list_res["result"]["isError"], false);
        let items = list_res["result"]["structuredContent"]["artifacts"]
            .as_array()
            .expect("artifacts array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], id);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
