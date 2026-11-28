//! Unified agent integration CLI (`kumu integration <install|uninstall|status>`).

use serde_json::json;

use super::cli::{print_json, usage_error};
use crate::agent::integration::{
    ComponentOutcome, McpState, Status, detailed_status, install_detailed, uninstall_detailed,
};
use crate::agent::{AGENTS, id_for, installed};

const INTEGRATION_USAGE: &str = "\
usage:
  kumu integration install [AGENT]
  kumu integration uninstall [AGENT]
  kumu integration status [--json]

Manages Kumu agent integrations (notifications/extensions, MCP, and skills).
Without AGENT, `install` targets agents detected on this machine, and
`uninstall` targets agents with any active Kumu registration.";

#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    Install(Option<&'a str>),
    Uninstall(Option<&'a str>),
    Status { json: bool },
    Help,
}

fn parse_args(args: &[String]) -> Result<Action<'_>, String> {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => Ok(Action::Help),
        Some("install") => match &args[1..] {
            [] => Ok(Action::Install(None)),
            [flag] if flag == "-h" || flag == "--help" => Ok(Action::Help),
            [agent] if !agent.starts_with('-') => Ok(Action::Install(Some(agent.as_str()))),
            [other] => Err(format!("unknown option {other}")),
            _ => Err("expected at most one agent id".into()),
        },
        Some("uninstall" | "remove") => match &args[1..] {
            [] => Ok(Action::Uninstall(None)),
            [flag] if flag == "-h" || flag == "--help" => Ok(Action::Help),
            [agent] if !agent.starts_with('-') => Ok(Action::Uninstall(Some(agent.as_str()))),
            [other] => Err(format!("unknown option {other}")),
            _ => Err("expected at most one agent id".into()),
        },
        Some("status") => {
            let mut json = false;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => return Ok(Action::Help),
                    other => return Err(format!("unknown option {other}")),
                }
            }
            Ok(Action::Status { json })
        }
        Some(other) => Err(format!("unknown integration subcommand {other}")),
    }
}

pub(super) fn integration_cli(args: &[String]) -> i32 {
    match parse_args(args) {
        Ok(Action::Help) => {
            println!("{INTEGRATION_USAGE}");
            0
        }
        Ok(Action::Install(target)) => run_install(target),
        Ok(Action::Uninstall(target)) => run_uninstall(target),
        Ok(Action::Status { json }) => run_status(json),
        Err(message) => usage_error(&message),
    }
}

fn run_install(target: Option<&str>) -> i32 {
    let targets: Vec<&'static str> = match target {
        Some(raw) => {
            let Some(id) = id_for(raw) else {
                return usage_error(&format!("unknown agent '{raw}'"));
            };
            if !installed(id) {
                eprintln!(
                    "note: '{id}' executable was not detected on PATH; installing integration anyway."
                );
            }
            vec![id]
        }
        None => {
            let detected: Vec<&'static str> = AGENTS
                .iter()
                .filter(|agent| installed(agent.id))
                .map(|agent| agent.id)
                .collect();
            if detected.is_empty() {
                println!("No supported coding agents detected on this machine.");
                return 0;
            }
            detected
        }
    };

    let mut failed = false;
    for agent in targets {
        let outcome = install_detailed(agent);
        let summary = format_outcome(&outcome);
        if outcome.is_ok() {
            if agent == "codex" {
                println!("{agent}: {summary} (restart Codex and review/trust hooks in /hooks)");
            } else {
                println!("{agent}: {summary}");
            }
        } else {
            eprintln!("{agent}: {summary}");
            failed = true;
        }
    }
    if failed { 1 } else { 0 }
}

fn run_uninstall(target: Option<&str>) -> i32 {
    let (targets, force_shared): (Vec<&'static str>, bool) = match target {
        Some(raw) => {
            let Some(id) = id_for(raw) else {
                return usage_error(&format!("unknown agent '{raw}'"));
            };
            (vec![id], false)
        }
        None => {
            let active: Vec<&'static str> = AGENTS
                .iter()
                .filter(|agent| detailed_status(agent.id).has_any_registration())
                .map(|agent| agent.id)
                .collect();
            if active.is_empty() {
                println!("No active Kumu agent integrations found.");
                return 0;
            }
            (active, true)
        }
    };

    let mut failed = false;
    for agent in targets {
        let outcome = uninstall_detailed(agent, force_shared);
        let summary = format_outcome(&outcome);
        if outcome.is_ok() {
            println!("{agent}: {summary}");
        } else {
            eprintln!("{agent}: {summary}");
            failed = true;
        }
    }
    if failed { 1 } else { 0 }
}

fn run_status(json_out: bool) -> i32 {
    let mut rows = Vec::with_capacity(AGENTS.len());
    for agent in AGENTS {
        let is_installed = installed(agent.id);
        let detail = detailed_status(agent.id);
        rows.push((agent.id, is_installed, detail));
    }

    if json_out {
        let payload: Vec<_> = rows
            .iter()
            .map(|(id, is_installed, detail)| {
                json!({
                    "agent": id,
                    "installed": is_installed,
                    "notify": format_status_json(&detail.notify),
                    "mcp": format_mcp_json(&detail.mcp),
                    "skills": format_status_json(&detail.skills),
                })
            })
            .collect();
        return print_json(&json!({ "agents": payload }));
    }

    println!(
        "{:<10} {:<10} {:<12} {:<14} SKILLS",
        "AGENT", "INSTALLED", "NOTIFY", "MCP"
    );
    for (id, is_installed, detail) in &rows {
        println!(
            "{:<10} {:<10} {:<12} {:<14} {}",
            id,
            if *is_installed { "yes" } else { "no" },
            format_status_text(&detail.notify),
            format_mcp_text(&detail.mcp),
            format_status_text(&detail.skills),
        );
    }
    0
}

fn format_outcome(outcome: &ComponentOutcome) -> String {
    let mut parts = Vec::with_capacity(3);
    parts.push(format_component("notify", &outcome.notify));
    if let Some(mcp) = &outcome.mcp {
        parts.push(format_component("mcp", mcp));
    }
    parts.push(format_component("skills", &outcome.skills));
    parts.join(", ")
}

fn format_component(label: &str, result: &Result<(), String>) -> String {
    match result {
        Ok(()) => format!("{label} ok"),
        Err(err) => format!("{label} failed: {err}"),
    }
}

fn status_str(status: Status) -> &'static str {
    match status {
        Status::Missing => "missing",
        Status::Registered => "registered",
        Status::NeedsRepair => "needs_repair",
    }
}

fn format_status_text(result: &Result<Status, String>) -> String {
    match result {
        Ok(status) => status_str(*status).to_string(),
        Err(err) => format!("error: {err}"),
    }
}

fn format_status_json(result: &Result<Status, String>) -> String {
    format_status_text(result)
}

fn format_mcp_text(state: &McpState) -> String {
    match state {
        McpState::Supported(res) => format_status_text(res),
        McpState::ExtensionPartial => "ext(partial)".to_string(),
    }
}

fn format_mcp_json(state: &McpState) -> String {
    match state {
        McpState::Supported(res) => format_status_json(res),
        McpState::ExtensionPartial => "extension_partial".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::integration::IntegrationDetail;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn parse_integration_cli_arguments() {
        assert_eq!(parse_args(&s(&[])), Ok(Action::Help));
        assert_eq!(parse_args(&s(&["install"])), Ok(Action::Install(None)));
        assert_eq!(
            parse_args(&s(&["install", "claude-code"])),
            Ok(Action::Install(Some("claude-code")))
        );
        assert_eq!(parse_args(&s(&["uninstall"])), Ok(Action::Uninstall(None)));
        assert_eq!(
            parse_args(&s(&["uninstall", "codex"])),
            Ok(Action::Uninstall(Some("codex")))
        );
        assert_eq!(
            parse_args(&s(&["status"])),
            Ok(Action::Status { json: false })
        );
        assert_eq!(
            parse_args(&s(&["status", "--json"])),
            Ok(Action::Status { json: true })
        );
        assert!(parse_args(&s(&["status", "--bogus"])).is_err());
        assert!(parse_args(&s(&["install", "claude", "extra"])).is_err());
        assert!(parse_args(&s(&["unknown"])).is_err());
    }

    #[test]
    fn format_outcome_summarizes_partial_failures() {
        let outcome = ComponentOutcome {
            notify: Ok(()),
            mcp: Some(Ok(())),
            skills: Err("conflict".into()),
        };
        assert!(!outcome.is_ok());
        assert_eq!(
            format_outcome(&outcome),
            "notify ok, mcp ok, skills failed: conflict"
        );
    }

    #[test]
    fn detail_detects_any_active_or_broken_component() {
        let empty = IntegrationDetail {
            notify: Ok(Status::Missing),
            mcp: McpState::ExtensionPartial,
            skills: Ok(Status::Missing),
        };
        assert!(!empty.has_any_registration());

        let skills_only = IntegrationDetail {
            notify: Ok(Status::Missing),
            mcp: McpState::Supported(Ok(Status::Missing)),
            skills: Ok(Status::Registered),
        };
        assert!(skills_only.has_any_registration());

        let broken_notify = IntegrationDetail {
            notify: Err("invalid json".into()),
            mcp: McpState::Supported(Ok(Status::Missing)),
            skills: Ok(Status::Missing),
        };
        assert!(broken_notify.has_any_registration());
    }
}
