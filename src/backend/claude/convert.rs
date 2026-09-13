//! Protocol -> app translation. Claude Code exposes no discovery request to
//! map yet, so everything here is a fixed vocabulary: the values `claude`
//! accepts on its own flags, which the pickers can therefore offer before any
//! session exists.
//!
//! The labels are kumu's own where the concept is kumu's to name (as with
//! Codex's sandbox policies) and the agent's own where they are a scale the
//! user types and reads in Claude Code itself.

use crate::backend::event::{
    AccessInfo, ModeInfo, ModelInfo, PermissionKind, PermissionOptionInfo, ReasoningInfo,
};
use serde_json::Value;

use super::protocol::{ControlRequestBody, ModelEntry};

/// The provider label Claude Code's models are filed under.
///
/// Claude Code publishes no model catalog and no per-model provider: `--model`
/// takes an alias, and which provider actually serves it — the Anthropic API,
/// Bedrock, Google Cloud's Agent Platform, Microsoft Foundry, or a gateway —
/// is decided by the account's own configuration, which kumu cannot see.
/// Labelling each alias by itself would file every one under its own
/// "provider" in the picker, so one constant is used instead. `modelProvider`
/// is never borrowed from a live session for the same reason Codex's backend
/// does not borrow its thread's: the picker would regroup the moment a session
/// started.
const PROVIDER: &str = "anthropic";

/// The model aliases kumu offers, in the order Claude Code's own model table
/// presents them.
///
/// Only the family aliases are listed, and three kinds of value are
/// deliberately absent:
///
///   - `default` is not a model but the sentinel that *clears* a model
///     override. kumu's own "nothing picked" already means "send no `--model`
///     at all", so offering it would overwrite the user's configured model
///     instead of inheriting it.
///   - `fable` and `best` are left out because a Fable request in print mode
///     bills usage credits without asking: the consent prompt an interactive
///     session shows is suppressed under `-p`, so a pick here could spend
///     money the user never agreed to.
///   - `sonnet[1m]`, `opus[1m]`, `opusplan`, `fable[1m]` and the rest are
///     qualifiers on a family alias rather than models of their own, and the
///     combinations cannot be enumerated from outside, so listing a subset
///     would read as a complete menu when it is not.
///
/// The labels stay version-free because that is what an alias means: it
/// resolves to whatever version the account's provider currently recommends.
pub(super) fn model_options() -> Vec<ModelInfo> {
    [("sonnet", "Sonnet"), ("opus", "Opus"), ("haiku", "Haiku")]
        .into_iter()
        .map(|(value, name)| ModelInfo {
            value: value.to_string(),
            name: name.to_string(),
            provider: PROVIDER.to_string(),
        })
        .collect()
}

/// The work modes kumu offers: what the agent is doing, as opposed to how much
/// it may do on its own ([`access_options`]).
///
/// The two values are the wire's own: `plan` is Claude Code's plan mode, and
/// `default` is the mode that defers to the session's configured `defaultMode` —
/// which is what "build" means here, since Claude Code has no flag for "the
/// normal working mode" beyond not being in plan. So a pick of 构建 asks for the
/// user's configured mode rather than kumu choosing one for them, and if that
/// configuration is itself `plan`, the session keeps reporting plan.
///
/// ACP agents report their own modes for this same axis (`build` / `plan`), which
/// is why it is one chip whichever agent a session runs.
pub(super) fn mode_options() -> Vec<ModeInfo> {
    [("default", "构建"), ("plan", "计划")]
        .into_iter()
        .map(|(value, name)| ModeInfo {
            value: value.to_string(),
            name: name.to_string(),
        })
        .collect()
}

/// The access modes kumu offers: a four-step ladder from most supervised to
/// least, which is what carries "how much may the agent do before it has to
/// ask?" for Claude.
///
/// These are Claude Code's permission modes minus `plan`, which belongs to the
/// other axis (see [`mode_options`]), and minus `dontAsk`: it is a real mode the
/// CLI accepts, but "deny whatever would have prompted" has no counterpart on
/// the other agents kumu drives, so it is not an offer here. A session a user
/// configured into either one still reports it — see [`current_access`] — so the
/// chip never claims nothing is in force when something is.
pub(super) fn access_options() -> Vec<AccessInfo> {
    [
        ("default", "监督"),
        ("acceptEdits", "自动接受编辑"),
        ("auto", "自动"),
        ("bypassPermissions", "跳过权限检查"),
    ]
    .into_iter()
    .map(|(value, name)| AccessInfo {
        value: value.to_string(),
        name: name.to_string(),
    })
    .collect()
}

/// The work mode in force: `plan` while the session is planning, `default`
/// (the value behind 构建) otherwise.
///
/// The CLI reports one value for both axes, so this is a projection: anything
/// that is not plan is the normal working mode.
pub(super) fn current_mode(reported: Option<&str>) -> Option<String> {
    let value = reported?.trim();
    (!value.is_empty()).then(|| {
        if value == "plan" {
            "plan".to_string()
        } else {
            "default".to_string()
        }
    })
}

/// The access policy in force, or `None` while nothing on that axis is in charge
/// — plan mode is the case that matters: it governs the session on its own, and
/// kumu has asked for no particular approval policy underneath it.
///
/// Deliberately unfiltered by [`access_options`] in the other direction: a value
/// the menu does not offer is still reported, so a session configured into
/// `dontAsk` says so rather than falling back to "nothing in force".
pub(super) fn current_access(reported: Option<&str>) -> Option<String> {
    let value = reported?.trim();
    (!value.is_empty() && value != "plan").then(|| value.to_string())
}

/// The reasoning efforts kumu offers.
///
/// Reported as one session-level list even though Claude Code documents the
/// available levels per model: an unsupported level clamps down to the highest
/// one at or below it rather than failing, so the union is safe to offer and
/// no per-model reduction is needed — which is what keeps this axis a single
/// list on both sides of the boundary.
///
/// The names stay as the levels themselves rather than being translated: this
/// is a scale the user passes to `--effort` and reads back in Claude Code's
/// own session header, so renaming it would break the correspondence with the
/// agent's documentation.
///
/// `ultracode` is not listed. It is a workflow mode rather than an effort
/// level: it runs at `xhigh` and additionally has Claude orchestrate dynamic
/// workflows, so offering it beside the levels would misdescribe it.
pub(super) fn reasoning_options() -> Vec<ReasoningInfo> {
    ["low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .map(|level| ReasoningInfo {
            value: level.to_string(),
            name: level.to_string(),
        })
        .collect()
}

/// A live session's picker catalogue, as the `initialize` response reports it,
/// falling back to the fixed vocabulary above when the report was empty — an
/// older CLI, or a shape this backend does not recognise, should still leave
/// the user something to pick.
pub(super) fn session_models(reported: &[ModelEntry]) -> Vec<ModelInfo> {
    if reported.is_empty() {
        return model_options();
    }
    reported
        .iter()
        .filter(|model| !model.value.is_empty())
        .map(|model| ModelInfo {
            value: model.value.clone(),
            // The CLI's label for the entry, else the model it resolves to: a
            // gateway points several values at one model, and naming that is
            // more use than repeating the value.
            name: model
                .display_name
                .clone()
                .filter(|name| !name.is_empty())
                .or_else(|| model.resolved_model.clone().filter(|name| !name.is_empty()))
                .unwrap_or_else(|| model.value.clone()),
            provider: PROVIDER.to_string(),
        })
        .collect()
}

/// The efforts a live session offers: the union of what its models advertise,
/// in first-seen order.
///
/// One list is safe because an unsupported level clamps down to the highest one
/// at or below it rather than failing. An empty report falls back to the
/// documented levels.
pub(super) fn session_reasoning(reported: &[ModelEntry]) -> Vec<ReasoningInfo> {
    let mut efforts: Vec<ReasoningInfo> = Vec::new();
    for model in reported.iter().filter(|model| model.supports_effort) {
        for level in &model.supported_effort_levels {
            if level.is_empty() || efforts.iter().any(|effort| effort.value == *level) {
                continue;
            }
            efforts.push(ReasoningInfo {
                value: level.clone(),
                name: level.clone(),
            });
        }
    }
    if efforts.is_empty() {
        reasoning_options()
    } else {
        efforts
    }
}

/// kumu's answers to a Claude Code permission request.
///
/// Two, not four: "always" is the CLI's own `permission_suggestions`, and those
/// are heterogeneous updates (`setMode` in the captured request, rule additions
/// in the Agent SDK's docs) whose shapes need their own verification. Echoing
/// one back is therefore left out rather than guessed at, and the permission
/// card's own cancel answer covers the rest.
pub(super) fn permission_options() -> Vec<PermissionOptionInfo> {
    vec![
        PermissionOptionInfo {
            id: ALLOW_OPTION.to_string(),
            name: "允许一次".to_string(),
            kind: PermissionKind::AllowOnce,
        },
        PermissionOptionInfo {
            id: DENY_OPTION.to_string(),
            name: "拒绝".to_string(),
            kind: PermissionKind::RejectOnce,
        },
    ]
}

/// The permission card's headline: the CLI's own one-line summary when it made
/// one, else the same title the tool row carries.
pub(super) fn permission_title(request: &ControlRequestBody) -> String {
    let tool = request
        .display_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&request.tool_name);
    match request.description.as_deref().filter(|d| !d.is_empty()) {
        Some(description) => format!("{tool} · {description}"),
        None => tool_title(&request.tool_name, &request.input),
    }
}

/// The permission card's body: the call's own parameters, pretty-printed, so
/// the prompt is answerable on its own.
pub(super) fn permission_detail(request: &ControlRequestBody) -> String {
    if request.input.is_null() {
        return String::new();
    }
    serde_json::to_string_pretty(&request.input).unwrap_or_default()
}

/// The row title for a tool call: Claude Code's tool name plus whatever makes
/// the call recognisable.
///
/// The name leads, unlike the Codex rows where the command is self-describing:
/// `Write` and `Read` differ by a name the user recognises from Claude Code
/// itself, not by the path they take.
pub(super) fn tool_title(name: &str, input: &Value) -> String {
    let field = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let detail = match name {
        "Bash" => field("command"),
        "Read" | "Write" | "Edit" => field("file_path"),
        "NotebookEdit" => field("notebook_path"),
        "Glob" | "Grep" => field("pattern"),
        "WebFetch" => field("url"),
        "WebSearch" => return format!("搜索：{}", field("query").unwrap_or("")),
        "Task" => field("description").or_else(|| field("subagent_type")),
        _ => None,
    };
    match detail {
        Some(detail) => format!("{name} · {detail}"),
        None => name.to_string(),
    }
}

/// A tool result's text. The CLI sends a string for most tools and an array of
/// content blocks for a few (and nothing at all for the rest), so all three
/// shapes flatten into the row's body.
pub(super) fn tool_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The id of the allow answer in [`permission_options`].
pub(super) const ALLOW_OPTION: &str = "allow-once";
/// The id of the refuse answer in [`permission_options`].
pub(super) const DENY_OPTION: &str = "deny-once";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every alias must be offered with a value `--model` accepts and a label
    /// distinct from the raw value, and they must all share one provider so the
    /// picker does not file each alias under a provider of its own.
    #[test]
    fn model_options_label_the_aliases_under_one_provider() {
        let models = model_options();
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["sonnet", "opus", "haiku"]
        );
        assert_eq!(
            models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["Sonnet", "Opus", "Haiku"]
        );
        assert!(
            models.iter().all(|m| m.provider == PROVIDER),
            "one provider label for the whole catalog"
        );
    }

    /// `default` is the sentinel that clears an override rather than a model,
    /// so it must not be offered: picking it would overwrite the user's
    /// configured model instead of inheriting it. `fable`/`best` are excluded
    /// because a Fable request bills usage credits without asking under `-p`.
    #[test]
    fn model_options_omit_the_default_sentinel_and_credit_billing_models() {
        let values = model_options()
            .into_iter()
            .map(|m| m.value)
            .collect::<Vec<_>>();
        for absent in ["default", "fable", "best", "opusplan", "sonnet[1m]"] {
            assert!(
                !values.iter().any(|v| v == absent),
                "{absent} must not be offered as a model"
            );
        }
    }

    /// The work mode axis is the one chip whose values are what the agent is
    /// doing, and `default` (构建) is the wire's way of saying "not planning":
    /// Claude Code has no separate flag for the normal working mode.
    #[test]
    fn work_modes_are_build_and_plan() {
        let modes = mode_options();
        assert_eq!(
            modes.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["default", "plan"]
        );
        assert_eq!(
            modes.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["构建", "计划"]
        );
    }

    /// The access axis is the four-step ladder, and `plan` is deliberately not
    /// one of its steps even though the CLI accepts it as a permission mode.
    #[test]
    fn access_options_are_the_four_step_ladder() {
        let policies = access_options();
        assert_eq!(
            policies
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>(),
            ["default", "acceptEdits", "auto", "bypassPermissions"]
        );
        assert!(
            !policies
                .iter()
                .any(|p| p.value == "plan" || p.value == "dontAsk"),
            "plan belongs to the work mode axis, and dontAsk is not offered"
        );
    }

    /// One reported value projects onto both axes: it is the work mode, and —
    /// unless it is plan, which governs on its own — the access policy in force.
    #[test]
    fn one_reported_mode_projects_onto_both_axes() {
        assert_eq!(current_mode(Some("plan")).as_deref(), Some("plan"));
        assert_eq!(
            current_access(Some("plan")),
            None,
            "plan mode leaves no approval policy in charge"
        );

        assert_eq!(
            current_mode(Some("acceptEdits")).as_deref(),
            Some("default"),
            "anything that is not plan is the normal working mode"
        );
        assert_eq!(
            current_access(Some("acceptEdits")).as_deref(),
            Some("acceptEdits")
        );

        // A mode the menu does not offer is still reported rather than dropped.
        assert_eq!(current_access(Some("dontAsk")).as_deref(), Some("dontAsk"));
        assert_eq!(current_mode(None), None);
        assert_eq!(current_access(None), None);
        assert_eq!(current_access(Some("  ")), None);
    }

    /// The levels are reported as the agent names them, in ascending order, and
    /// `ultracode` is not among them because it is a workflow mode rather than
    /// an effort level.
    #[test]
    fn reasoning_options_keep_the_agents_level_names() {
        let efforts = reasoning_options();
        assert_eq!(
            efforts.iter().map(|e| e.value.as_str()).collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(
            efforts
                .iter()
                .all(|e| e.name == e.value && e.value != "ultracode"),
            "the value is the label, and ultracode is not a level"
        );
    }

    fn reported_model(value: &str, resolved: &str, levels: &[&str]) -> ModelEntry {
        ModelEntry {
            value: value.into(),
            resolved_model: Some(resolved.into()),
            display_name: Some(resolved.into()),
            supports_effort: !levels.is_empty(),
            supported_effort_levels: levels.iter().map(|l| l.to_string()).collect(),
        }
    }

    /// A session's own report replaces the fixed vocabulary: the values are the
    /// ones the account accepts, and their labels are the resolved models.
    #[test]
    fn a_session_reports_its_own_model_catalogue() {
        let models = session_models(&[
            reported_model("default", "gateway-model", &["low", "high"]),
            reported_model("haiku", "gateway-fast", &["low"]),
        ]);
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["default", "haiku"]
        );
        assert_eq!(models[0].name, "gateway-model");
        assert!(models.iter().all(|m| m.provider == PROVIDER));
    }

    /// An empty report is not an empty picker: the documented aliases stand in.
    #[test]
    fn an_empty_report_falls_back_to_the_documented_vocabulary() {
        assert_eq!(
            session_models(&[])
                .iter()
                .map(|m| m.value.as_str())
                .collect::<Vec<_>>(),
            ["sonnet", "opus", "haiku"]
        );
        assert_eq!(
            session_reasoning(&[])
                .iter()
                .map(|e| e.value.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh", "max"]
        );
    }

    /// The efforts are the union across models, in the order they were
    /// advertised, without duplicates — a model that supports fewer levels does
    /// not shrink the list.
    #[test]
    fn session_efforts_are_the_union_across_models() {
        let efforts = session_reasoning(&[
            reported_model("default", "m", &["low", "high"]),
            reported_model("haiku", "f", &["high", "max"]),
            reported_model("plain", "p", &[]),
        ]);
        assert_eq!(
            efforts.iter().map(|e| e.value.as_str()).collect::<Vec<_>>(),
            ["low", "high", "max"]
        );
    }

    /// A permission request gets exactly two answers, and the allow id is the
    /// one the backend compares against.
    #[test]
    fn permission_options_offer_allow_once_and_refuse() {
        let options = permission_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id, ALLOW_OPTION);
        assert_eq!(options[0].kind, PermissionKind::AllowOnce);
        assert_eq!(options[1].id, DENY_OPTION);
        assert_eq!(options[1].kind, PermissionKind::RejectOnce);
    }

    /// Tool rows lead with the tool's name and then the recognisable part of
    /// the call, and a call with nothing to add is just the name.
    #[test]
    fn tool_titles_name_the_tool_and_its_subject() {
        assert_eq!(
            tool_title("Bash", &json!({"command": "ls -la"})),
            "Bash · ls -la"
        );
        assert_eq!(
            tool_title("Write", &json!({"file_path": "/tmp/a.txt", "content": "x"})),
            "Write · /tmp/a.txt"
        );
        assert_eq!(
            tool_title("WebSearch", &json!({"query": "rust"})),
            "搜索：rust"
        );
        assert_eq!(
            tool_title("Task", &json!({"description": "find it"})),
            "Task · find it"
        );
        assert_eq!(tool_title("TodoWrite", &json!({"todos": []})), "TodoWrite");
    }

    /// A result arrives as a string, as blocks, or not at all.
    #[test]
    fn tool_results_flatten_every_shape() {
        assert_eq!(tool_content(&json!("done")), "done");
        assert_eq!(
            tool_content(&json!([{"type": "text", "text": "a"}, {"type": "image"}])),
            "a"
        );
        assert_eq!(tool_content(&Value::Null), "");
        assert_eq!(tool_content(&json!(42)), "42");
    }

    /// The permission card leads with the CLI's summary when it made one, and
    /// falls back to the tool row's title when it did not.
    #[test]
    fn permission_titles_prefer_the_clis_own_summary() {
        let with_summary = ControlRequestBody {
            subtype: "can_use_tool".into(),
            tool_name: "Write".into(),
            display_name: Some("Write".into()),
            description: Some("probe.txt".into()),
            input: json!({"file_path": "/tmp/probe.txt"}),
            tool_use_id: Some("call_1".into()),
        };
        assert_eq!(permission_title(&with_summary), "Write · probe.txt");
        assert!(permission_detail(&with_summary).contains("/tmp/probe.txt"));

        let without_summary = ControlRequestBody {
            description: None,
            ..with_summary
        };
        assert_eq!(
            permission_title(&without_summary),
            format!("Write · /tmp/probe.txt")
        );

        let no_input = ControlRequestBody {
            input: Value::Null,
            ..without_summary
        };
        assert_eq!(permission_detail(&no_input), "");
    }
}
