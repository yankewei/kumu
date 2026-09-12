//! Protocol -> app translation for the Codex backend. Everything the UI
//! consumes is a plain value produced here, so app-server shapes stop at this
//! boundary — the same rule the ACP backend keeps.
//!
//! The mapping constants matter: an unfamiliar status becomes "this update did
//! not change the row" rather than being mistaken for a failure or a running
//! tool call, because app-server is free to add statuses between releases.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::backend::event::{
    Event, ModelInfo, PermissionKind, PermissionOptionInfo, ReasoningInfo, SandboxInfo,
};

use super::protocol::{
    FileUpdateChange, McpToolCallError, McpToolCallResult, ModelListResult, SandboxPolicy,
    ThreadItem,
};

/// The provider label Codex models are filed under.
///
/// `model/list` reports no provider, and borrowing the thread's
/// `modelProvider` instead would make the picker regroup the moment a session
/// started, so the probe and a live session label alike.
const PROVIDER: &str = "openai";

/// Map a Codex item status onto kumu's row vocabulary (`pending`/`running`/
/// `done`/`failed`). An unfamiliar value maps to empty, which the UI reads as
/// "leave the row's status alone".
pub(super) fn status_name(status: &str) -> &'static str {
    match status {
        "inProgress" => "running",
        "completed" => "done",
        "failed" | "declined" => "failed",
        _ => "",
    }
}

/// The label a tool row shows for an item.
fn tool_title(item: &ThreadItem) -> String {
    match item {
        ThreadItem::CommandExecution { command, .. } => command.clone(),
        ThreadItem::FileChange { changes, .. } => {
            let paths = changes
                .iter()
                .map(|change| change.path.as_str())
                .filter(|path| !path.is_empty())
                .collect::<Vec<_>>();
            if paths.is_empty() {
                "文件改动".to_string()
            } else {
                paths.join(", ")
            }
        }
        ThreadItem::McpToolCall { server, tool, .. } => format!("{server}/{tool}"),
        ThreadItem::WebSearch { query, .. } => format!("搜索：{query}"),
        _ => String::new(),
    }
}

/// The body a tool row shows: a diff for file changes, output for commands.
pub(super) fn item_content(item: &ThreadItem) -> String {
    match item {
        ThreadItem::CommandExecution {
            aggregated_output, ..
        } => aggregated_output.clone().unwrap_or_default(),
        ThreadItem::FileChange { changes, .. } => render_changes(changes),
        ThreadItem::McpToolCall { error, result, .. } => {
            mcp_content(error.as_ref(), result.as_ref())
        }
        _ => String::new(),
    }
}

/// Render proposed file changes: each path followed by its diff. Codex already
/// supplies the diff with `+`/`-` markers, so unlike the ACP path there is
/// nothing to reconstruct.
fn render_changes(changes: &[FileUpdateChange]) -> String {
    changes
        .iter()
        .map(
            |change| match (change.path.is_empty(), change.diff.is_empty()) {
                (false, false) => format!("{}\n{}", change.path, change.diff.trim_end()),
                (false, true) => change.path.clone(),
                (true, false) => change.diff.trim_end().to_string(),
                (true, true) => String::new(),
            },
        )
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of an MCP tool call: its error, else the text content items it
/// returned (the rest are MCP's own shapes and stay opaque).
fn mcp_content(error: Option<&McpToolCallError>, result: Option<&McpToolCallResult>) -> String {
    if let Some(error) = error {
        return error.message.clone();
    }
    let Some(result) = result else {
        return String::new();
    };
    let Some(Value::Array(items)) = &result.content else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool row an item produces, or `None` for items kumu does not render —
/// messages and reasoning arrive as streamed text instead.
pub(super) fn tool_event(item: &ThreadItem) -> Option<Event> {
    let (id, status) = match item {
        ThreadItem::CommandExecution { id, status, .. }
        | ThreadItem::FileChange { id, status, .. }
        | ThreadItem::McpToolCall { id, status, .. } => (id, status.as_str()),
        // A search has no status of its own; it is only reported once done.
        ThreadItem::WebSearch { id, .. } => (id, "completed"),
        _ => return None,
    };
    if id.is_empty() {
        return None;
    }
    Some(Event::ToolCall {
        id: id.clone(),
        title: tool_title(item),
        status: status_name(status),
        content: item_content(item),
    })
}

/// Build the picker catalog from `model/list`: its visible models, and the one
/// the agent recommends as the active value.
pub(super) fn model_catalog(listed: &ModelListResult) -> (Option<String>, Vec<ModelInfo>) {
    let mut models = Vec::new();
    let mut default = None;
    for model in &listed.data {
        if model.hidden {
            continue;
        }
        if model.is_default && default.is_none() {
            default = Some(model.model.clone());
        }
        models.push(ModelInfo {
            value: model.model.clone(),
            name: if model.display_name.is_empty() {
                model.model.clone()
            } else {
                model.display_name.clone()
            },
            provider: PROVIDER.to_string(),
        });
    }
    models.sort_by(|a, b| a.name.cmp(&b.name));
    // An agent that names no default still needs something for the chip.
    if default.is_none() {
        default = models.first().map(|model| model.value.clone());
    }
    (default, models)
}

/// A model's reasoning axis: the efforts it advertises and the one the agent
/// uses absent an override.
///
/// Deliberately kept inside the backend. ACP's reasoning axis is session-scoped,
/// so the boundary carries a session-level list and each backend does its own
/// reduction — Codex's per-model shape never reaches the UI.
#[derive(Clone, Debug, Default)]
pub(super) struct ModelReasoning {
    pub efforts: Vec<String>,
    pub default: Option<String>,
}

/// The reasoning axis of every visible model, keyed by the value `turn/start`
/// takes. `model/list` is the only source: Codex advertises the set per model.
pub(super) fn model_reasoning(listed: &ModelListResult) -> HashMap<String, ModelReasoning> {
    listed
        .data
        .iter()
        .filter(|model| !model.hidden)
        .map(|model| {
            (
                model.model.clone(),
                ModelReasoning {
                    efforts: model
                        .supported_reasoning_efforts
                        .iter()
                        .map(|effort| effort.reasoning_effort.clone())
                        .filter(|effort| !effort.is_empty())
                        .collect(),
                    default: model
                        .default_reasoning_effort
                        .clone()
                        .filter(|effort| !effort.is_empty()),
                },
            )
        })
        .collect()
}

/// The session-level list the picker offers for a model: its advertised efforts,
/// in the agent's order. Empty when the model is unknown or advertises none,
/// which is what keeps the chip off the toolbar.
pub(super) fn reasoning_options(
    reasoning: &HashMap<String, ModelReasoning>,
    model: Option<&str>,
) -> Vec<ReasoningInfo> {
    let Some(model) = model.and_then(|value| reasoning.get(value)) else {
        return Vec::new();
    };
    model
        .efforts
        .iter()
        .map(|effort| ReasoningInfo {
            value: effort.clone(),
            // Codex describes an effort but gives it no short name, so the
            // value is the best label there is.
            name: effort.clone(),
        })
        .collect()
}

/// Whether a model advertises an effort. A pick that belonged to another model
/// is dropped rather than sent for the agent to reject.
pub(super) fn supports_effort(
    reasoning: &HashMap<String, ModelReasoning>,
    model: Option<&str>,
    effort: Option<&str>,
) -> bool {
    let (Some(model), Some(effort)) = (model, effort) else {
        return false;
    };
    reasoning
        .get(model)
        .is_some_and(|model| model.efforts.iter().any(|value| value == effort))
}

/// The effort in force for a model when the user has picked none: the model's
/// own default — but only for a model that advertises efforts at all, so a
/// default is never reported for a model with no such axis.
pub(super) fn default_effort(
    reasoning: &HashMap<String, ModelReasoning>,
    model: Option<&str>,
) -> Option<String> {
    let model = model.and_then(|value| reasoning.get(value))?;
    if model.efforts.is_empty() {
        return None;
    }
    model.default.clone()
}

/// The sandbox policies kumu offers for Codex, ordered least to most access so
/// the menu reads as a ladder.
///
/// The values are Codex's own wire strings, which is what lets a pick be echoed
/// back verbatim.
pub(super) fn sandbox_options() -> Vec<SandboxInfo> {
    [
        ("read-only", "只读"),
        ("workspace-write", "工作区写入"),
        ("danger-full-access", "完全访问"),
    ]
    .into_iter()
    .map(|(value, name)| SandboxInfo {
        value: value.to_string(),
        name: name.to_string(),
    })
    .collect()
}

/// The picker value for the policy a thread reports it is running under.
///
/// `None` for a policy kumu does not offer, so the chip falls back to its
/// inherited label instead of claiming a value that is not in the menu.
pub(super) fn sandbox_mode_value(policy: &SandboxPolicy) -> Option<&'static str> {
    match policy {
        SandboxPolicy::ReadOnly => Some("read-only"),
        SandboxPolicy::WorkspaceWrite => Some("workspace-write"),
        SandboxPolicy::DangerFullAccess => Some("danger-full-access"),
        SandboxPolicy::ExternalSandbox | SandboxPolicy::Other => None,
    }
}

/// The `turn/start.sandboxPolicy` payload for a picker value.
///
/// This exists to hide a real asymmetry in the protocol: `thread/start` takes a
/// kebab-case string (`"read-only"`), while `turn/start` takes a camelCase
/// tagged object (`{"type":"readOnly"}`). An unrecognised value yields `None`
/// so the caller omits the field — silently widening access would be the worst
/// possible failure mode here.
pub(super) fn sandbox_policy(value: &str) -> Option<Value> {
    let kind = match value {
        "read-only" => "readOnly",
        "workspace-write" => "workspaceWrite",
        "danger-full-access" => "dangerFullAccess",
        _ => return None,
    };
    Some(json!({ "type": kind }))
}

/// The decisions kumu offers for an approval.
///
/// app-server sends no option list — the decision vocabulary is fixed by the
/// protocol — so the labels are kumu's own and the ids are the wire values that
/// get echoed back verbatim.
pub(super) fn approval_options() -> Vec<PermissionOptionInfo> {
    vec![
        PermissionOptionInfo {
            id: "accept".to_string(),
            name: "允许一次".to_string(),
            kind: PermissionKind::AllowOnce,
        },
        PermissionOptionInfo {
            id: "acceptForSession".to_string(),
            name: "本次会话都允许".to_string(),
            kind: PermissionKind::AllowAlways,
        },
        PermissionOptionInfo {
            id: "decline".to_string(),
            name: "拒绝".to_string(),
            kind: PermissionKind::RejectOnce,
        },
        PermissionOptionInfo {
            id: "cancel".to_string(),
            name: "取消本轮".to_string(),
            kind: PermissionKind::Other,
        },
    ]
}

/// The decision value to send back for a chosen option.
///
/// Anything unrecognised — including no choice at all, which is what a turn
/// abandoned before the user decided looks like — becomes `cancel`: the one
/// answer that can never be read as consent.
pub(super) fn decision(option_id: Option<&str>) -> &'static str {
    match option_id {
        Some("accept") => "accept",
        Some("acceptForSession") => "acceptForSession",
        Some("decline") => "decline",
        _ => "cancel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statuses_map_onto_the_row_vocabulary() {
        assert_eq!(status_name("inProgress"), "running");
        assert_eq!(status_name("completed"), "done");
        assert_eq!(status_name("failed"), "failed");
        assert_eq!(status_name("declined"), "failed");
        // Unknown: leave the row alone rather than lie about it.
        assert_eq!(status_name("somethingNew"), "");
    }

    #[test]
    fn a_command_item_becomes_a_running_row() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "commandExecution",
            "id": "c1",
            "command": "cargo test",
            "cwd": "/repo",
            "status": "inProgress",
            "aggregatedOutput": "running...",
        }))
        .unwrap();
        let Event::ToolCall {
            id,
            title,
            status,
            content,
        } = tool_event(&item).expect("a tool row")
        else {
            panic!("expected a tool call");
        };
        assert_eq!(id, "c1");
        assert_eq!(title, "cargo test");
        assert_eq!(status, "running");
        assert_eq!(content, "running...");
    }

    #[test]
    fn a_file_change_item_shows_its_paths_and_diff() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "fileChange",
            "id": "f1",
            "status": "completed",
            "changes": [
                {"path": "src/main.rs", "diff": "@@\n-old\n+new", "kind": {"type": "update"}},
            ],
        }))
        .unwrap();
        let content = item_content(&item);
        assert!(content.contains("src/main.rs"), "missing path: {content}");
        assert!(content.contains("+new"), "missing diff: {content}");
        let Event::ToolCall { title, status, .. } = tool_event(&item).expect("a tool row") else {
            panic!("expected a tool call");
        };
        assert_eq!(title, "src/main.rs");
        assert_eq!(status, "done");
    }

    /// Messages and reasoning must not produce tool rows; they travel as
    /// streamed text.
    #[test]
    fn non_tool_items_produce_no_row() {
        for raw in [
            json!({"type": "agentMessage", "id": "a1", "text": "hi"}),
            json!({"type": "reasoning", "id": "r1"}),
            json!({"type": "somethingNew", "id": "x1"}),
        ] {
            let item: ThreadItem = serde_json::from_value(raw).unwrap();
            assert!(tool_event(&item).is_none(), "unexpected row for {item:?}");
        }
    }

    #[test]
    fn the_catalog_skips_hidden_models_and_names_a_default() {
        let listed: ModelListResult = serde_json::from_value(json!({
            "data": [
                {"model": "b", "displayName": "Beta", "hidden": false, "isDefault": true},
                {"model": "a", "displayName": "Alpha", "hidden": false, "isDefault": false},
                {"model": "h", "displayName": "Hidden", "hidden": true, "isDefault": false},
            ]
        }))
        .unwrap();
        let (current, models) = model_catalog(&listed);
        assert_eq!(current.as_deref(), Some("b"));
        assert_eq!(
            models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["Alpha", "Beta"],
            "hidden models are dropped and the rest are sorted by name"
        );
        assert_eq!(models[0].provider, "openai");
    }

    /// The reasoning catalogue is kept per model inside the backend, keyed by
    /// the value a turn takes, with the agent's order and default kept.
    fn reasoning_catalog() -> ModelListResult {
        serde_json::from_value(json!({
            "data": [
                {"model": "m", "displayName": "M", "hidden": false, "isDefault": true,
                 "defaultReasoningEffort": "medium",
                 "supportedReasoningEfforts": [
                     {"reasoningEffort": "low", "description": "d"},
                     {"reasoningEffort": "medium", "description": "d"},
                     {"reasoningEffort": "xhigh", "description": "d"}]},
                {"model": "plain", "displayName": "Plain", "hidden": false, "isDefault": false},
                {"model": "hidden", "displayName": "Hidden", "hidden": true, "isDefault": false,
                 "defaultReasoningEffort": "low",
                 "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "d"}]},
            ]
        }))
        .unwrap()
    }

    /// The session-level list comes from the selected model, and a model with no
    /// efforts yields none — the source of "no reasoning chip here".
    #[test]
    fn reasoning_options_follow_the_model() {
        let reasoning = model_reasoning(&reasoning_catalog());

        assert_eq!(
            reasoning_options(&reasoning, Some("m"))
                .iter()
                .map(|effort| effort.value.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "xhigh"],
            "the agent's order is kept"
        );
        assert_eq!(
            reasoning_options(&reasoning, Some("m"))[0].name,
            "low",
            "codex gives an effort no name, so the value is the label"
        );
        assert!(reasoning_options(&reasoning, Some("plain")).is_empty());
        assert!(reasoning_options(&reasoning, Some("gone")).is_empty());
        assert!(reasoning_options(&reasoning, None).is_empty());
        assert!(
            !reasoning.contains_key("hidden"),
            "a hidden model's efforts are not offered"
        );
    }

    /// A pick is only sent when the model in force actually advertises it, so a
    /// pick left over from another model is dropped instead of sent.
    #[test]
    fn an_unsupported_effort_is_not_sendable() {
        let reasoning = model_reasoning(&reasoning_catalog());

        assert!(supports_effort(&reasoning, Some("m"), Some("low")));
        assert!(
            !supports_effort(&reasoning, Some("m"), Some("ultra")),
            "a value this model never advertised"
        );
        assert!(
            !supports_effort(&reasoning, Some("plain"), Some("low")),
            "a model with no efforts at all"
        );
        assert!(!supports_effort(&reasoning, Some("gone"), Some("low")));
        assert!(!supports_effort(&reasoning, Some("m"), None));
        assert!(!supports_effort(&reasoning, None, Some("low")));
    }

    /// The fallback effort is the model's own default, and is never reported for
    /// a model with no reasoning axis.
    #[test]
    fn the_default_effort_is_the_models_own() {
        let reasoning = model_reasoning(&reasoning_catalog());

        assert_eq!(
            default_effort(&reasoning, Some("m")).as_deref(),
            Some("medium")
        );
        assert_eq!(default_effort(&reasoning, Some("plain")), None);
        assert_eq!(default_effort(&reasoning, Some("gone")), None);
        assert_eq!(default_effort(&reasoning, None), None);
    }

    #[test]
    fn a_catalog_without_a_default_falls_back_to_the_first_model() {
        let listed: ModelListResult = serde_json::from_value(json!({
            "data": [{"model": "only", "displayName": "Only", "hidden": false, "isDefault": false}]
        }))
        .unwrap();
        let (current, _) = model_catalog(&listed);
        assert_eq!(current.as_deref(), Some("only"));
    }

    /// The menu is a ladder from least to most access, and its values are the
    /// wire strings so a pick can be echoed back unchanged.
    #[test]
    fn sandbox_options_ladder_from_read_only_to_full_access() {
        let options = sandbox_options();
        assert_eq!(
            options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            ["read-only", "workspace-write", "danger-full-access"]
        );
        assert_eq!(options[0].name, "只读");
    }

    /// The two serialisations of the same policy: a kebab-case string for
    /// `thread/start`, a camelCase tagged object for `turn/start`. Getting this
    /// pair wrong is silent, so it is pinned here.
    #[test]
    fn sandbox_values_map_onto_both_wire_forms() {
        assert_eq!(
            sandbox_policy("read-only"),
            Some(json!({"type": "readOnly"}))
        );
        assert_eq!(
            sandbox_policy("workspace-write"),
            Some(json!({"type": "workspaceWrite"}))
        );
        assert_eq!(
            sandbox_policy("danger-full-access"),
            Some(json!({"type": "dangerFullAccess"}))
        );
    }

    /// A value kumu does not recognise must not become a policy of its own:
    /// omitting the field leaves the agent's configuration in charge, whereas
    /// guessing could quietly widen access.
    #[test]
    fn an_unknown_sandbox_value_is_not_sent() {
        assert_eq!(sandbox_policy("somethingNew"), None);
        assert_eq!(sandbox_policy(""), None);
    }

    /// The agent's echo is read back into the picker's own values, and an echo
    /// kumu has no value for is reported as unknown rather than mapped onto the
    /// nearest one.
    #[test]
    fn the_agents_sandbox_echo_maps_back() {
        let parse = |raw: &str| -> Option<&'static str> {
            let policy: SandboxPolicy = serde_json::from_str(raw).unwrap();
            sandbox_mode_value(&policy)
        };
        assert_eq!(parse(r#"{"type":"readOnly"}"#), Some("read-only"));
        assert_eq!(
            parse(r#"{"type":"workspaceWrite"}"#),
            Some("workspace-write")
        );
        assert_eq!(
            parse(r#"{"type":"dangerFullAccess"}"#),
            Some("danger-full-access")
        );
        assert_eq!(parse(r#"{"type":"externalSandbox"}"#), None);
        assert_eq!(parse(r#"{"type":"somethingNew"}"#), None);
    }

    #[test]
    fn approvals_offer_the_protocols_decisions() {
        let ids = approval_options()
            .iter()
            .map(|option| option.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["accept", "acceptForSession", "decline", "cancel"]);
    }

    /// Only the two affirmative answers can reach the agent as consent.
    #[test]
    fn an_unknown_choice_becomes_cancel() {
        assert_eq!(decision(Some("accept")), "accept");
        assert_eq!(decision(Some("acceptForSession")), "acceptForSession");
        assert_eq!(decision(Some("decline")), "decline");
        assert_eq!(decision(Some("cancel")), "cancel");
        assert_eq!(decision(None), "cancel");
        assert_eq!(decision(Some("somethingNew")), "cancel");
    }
}
