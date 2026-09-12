//! Protocol -> app translation. Everything the UI consumes is a plain value
//! produced here, so protocol shapes (`SessionUpdate`, tool-call content of
//! four different forms, permission option kinds) stop at this boundary.
//!
//! The mapping constants matter: unknown variants are mapped to neutral values
//! rather than being mistaken for a rejection or a running tool call.

use agent_client_protocol::schema::v1::{
    ContentBlock, Diff, PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionConfigSelectOptions, SessionUpdate, ToolCallContent,
    ToolCallStatus, ToolCallUpdate,
};

use crate::backend::event::{ModeInfo, ModelInfo, PermissionKind, ReasoningInfo};

/// A select session config option, located by [`select`].
struct Select<'a> {
    id: &'a SessionConfigId,
    current: &'a str,
    options: &'a SessionConfigSelectOptions,
}

/// Find the select config option for `category`, falling back to the option
/// whose id is `fallback_id`.
///
/// The category is what the spec reserves for a given selector (`model`,
/// `mode`); the id fallback covers agents that omit it, since categories are
/// explicitly "UX purposes only" and must be handled gracefully when missing.
fn select<'a>(
    config_options: &'a Option<Vec<SessionConfigOption>>,
    category: SessionConfigOptionCategory,
    fallback_id: &str,
) -> Option<Select<'a>> {
    for option in config_options.as_deref()? {
        let matches =
            option.category.as_ref() == Some(&category) || option.id.0.as_ref() == fallback_id;
        if !matches {
            continue;
        }
        if let SessionConfigKind::Select(select) = &option.kind {
            return Some(Select {
                id: &option.id,
                current: select.current_value.0.as_ref(),
                options: &select.options,
            });
        }
    }
    None
}

/// Flatten a select's values, grouped or not, keeping the agent's order.
fn select_values(options: &SessionConfigSelectOptions) -> Vec<(&str, &str)> {
    let mut values = Vec::new();
    match options {
        SessionConfigSelectOptions::Ungrouped(flat) => {
            for option in flat {
                values.push(select_value(option));
            }
        }
        SessionConfigSelectOptions::Grouped(groups) => {
            for group in groups {
                for option in &group.options {
                    values.push(select_value(option));
                }
            }
        }
        _ => {}
    }
    values
}

/// One select value as `(value, name)`; an option's optional description is
/// not carried through, since neither picker shows one.
fn select_value(option: &SessionConfigSelectOption) -> (&str, &str) {
    (option.value.0.as_ref(), option.name.as_str())
}

/// Extract the session's model config option: its id (for
/// `session/set_config_option`), the value the agent reports as active, and
/// the selectable models, grouped flat.
///
/// Models are sorted by provider then name so the picker groups them — the one
/// place kumu overrides the agent's own ordering.
pub(super) fn model_catalog(
    config_options: &Option<Vec<SessionConfigOption>>,
) -> (Option<SessionConfigId>, Option<String>, Vec<ModelInfo>) {
    let Some(select) = select(config_options, SessionConfigOptionCategory::Model, "model") else {
        return (None, None, Vec::new());
    };
    let mut models = select_values(select.options)
        .into_iter()
        .map(|(value, name)| {
            let provider = value.split('/').next().unwrap_or(value).to_string();
            ModelInfo {
                value: value.to_string(),
                name: strip_provider_prefix(name, &provider),
                provider,
            }
        })
        .collect::<Vec<_>>();
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
    (
        Some(select.id.clone()),
        Some(select.current.to_string()),
        models,
    )
}

/// Extract the session's mode config option: its id (for
/// `session/set_config_option`), the value the agent reports as active, and
/// the selectable modes.
///
/// Modes keep the agent's own order, unlike models: the spec asks clients to
/// display options in the order given, and that order carries intent.
pub(super) fn mode_catalog(
    config_options: &Option<Vec<SessionConfigOption>>,
) -> (Option<SessionConfigId>, Option<String>, Vec<ModeInfo>) {
    let Some(select) = select(config_options, SessionConfigOptionCategory::Mode, "mode") else {
        return (None, None, Vec::new());
    };
    let modes = select_values(select.options)
        .into_iter()
        .map(|(value, name)| ModeInfo {
            value: value.to_string(),
            name: name.to_string(),
        })
        .collect();
    (
        Some(select.id.clone()),
        Some(select.current.to_string()),
        modes,
    )
}

/// Extract the session's reasoning-effort config option (`thought_level`): its
/// id (for `session/set_config_option`), the value the agent reports as active,
/// and the selectable efforts.
///
/// Unlike model and mode this has no id fallback: the category is the only
/// stable name, because agents call the option whatever they like (`thinking`
/// for omp, `effort` for opencode). An agent that exposes no such option simply
/// has no reasoning axis.
pub(super) fn reasoning_catalog(
    config_options: &Option<Vec<SessionConfigOption>>,
) -> (Option<SessionConfigId>, Option<String>, Vec<ReasoningInfo>) {
    let Some(select) = select(
        config_options,
        SessionConfigOptionCategory::ThoughtLevel,
        "",
    ) else {
        return (None, None, Vec::new());
    };
    let efforts = select_values(select.options)
        .into_iter()
        .map(|(value, name)| ReasoningInfo {
            value: value.to_string(),
            name: name.to_string(),
        })
        .collect();
    (
        Some(select.id.clone()),
        Some(select.current.to_string()),
        efforts,
    )
}

/// Drop the provider prefix from a display name ("OpenCode Go/GLM-5.3" →
/// "GLM-5.3"): agents like opencode concatenate provider + model in the name,
/// which duplicates the provider column in the picker. The prefix only counts
/// as a provider when it normalizes to the same key as the value's provider
/// segment; otherwise the name is kept untouched.
fn strip_provider_prefix(name: &str, provider: &str) -> String {
    let key = |s: &str| {
        s.to_lowercase()
            .chars()
            .filter(|c: &char| c.is_alphanumeric())
            .collect::<String>()
    };
    match name.split_once('/') {
        Some((prefix, rest)) if !rest.is_empty() && key(prefix) == key(provider) => {
            rest.to_string()
        }
        _ => name.to_string(),
    }
}

/// Stable lowercase names for tool call statuses (used by the UI rows).
pub(super) fn status_name(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "done",
        ToolCallStatus::Failed => "failed",
        _ => "pending",
    }
}

/// Classify a permission option for the UI. Unknown kinds (the enum is
/// `#[non_exhaustive]`) map to [`PermissionKind::Other`] so they render
/// neutrally instead of being mislabelled as a rejection.
pub(super) fn permission_kind(kind: &PermissionOptionKind) -> PermissionKind {
    match kind {
        PermissionOptionKind::AllowOnce => PermissionKind::AllowOnce,
        PermissionOptionKind::AllowAlways => PermissionKind::AllowAlways,
        PermissionOptionKind::RejectOnce => PermissionKind::RejectOnce,
        PermissionOptionKind::RejectAlways => PermissionKind::RejectAlways,
        _ => PermissionKind::Other,
    }
}

/// Turn the user's choice into the protocol outcome. `None` means the turn
/// ended (or was abandoned) before the user decided, which the spec requires
/// clients to answer with `Cancelled`.
pub(super) fn permission_outcome(option_id: Option<String>) -> RequestPermissionOutcome {
    match option_id {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// The detail shown in a permission prompt, so the user can inspect what they
/// are approving.
///
/// Agents describe the pending operation either as content blocks (text, file
/// diffs) or as `raw_input`; neither is guaranteed, and either may be the only
/// one present. Content wins when available because it is what the agent chose
/// to show about the change itself.
pub(super) fn permission_detail(tool_call: &ToolCallUpdate) -> String {
    let mut parts = Vec::new();
    if let Some(content) = &tool_call.fields.content {
        for block in content {
            match block {
                ToolCallContent::Content(c) => {
                    if let ContentBlock::Text(t) = &c.content {
                        parts.push(t.text.clone());
                    }
                }
                ToolCallContent::Diff(diff) => parts.push(render_diff(diff)),
                // A terminal id is not something a prompt can show inline.
                _ => {}
            }
        }
    }
    if !parts.is_empty() {
        return parts.join("\n");
    }
    tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_default()
}

/// Render a file modification as +/- lines for review before approval.
fn render_diff(diff: &Diff) -> String {
    let mut out = format!("{}\n", diff.path.display());
    if let Some(old) = &diff.old_text {
        for line in old.lines() {
            out.push_str(&format!("- {line}\n"));
        }
    }
    for line in diff.new_text.lines() {
        out.push_str(&format!("+ {line}\n"));
    }
    out.trim_end().to_string()
}

/// Flatten a tool call's content blocks into one text string, falling back to
/// the pretty-printed raw output when there are no text blocks.
pub(super) fn tool_call_content(
    content: &[ToolCallContent],
    raw_output: &Option<serde_json::Value>,
) -> String {
    let mut parts = Vec::new();
    for c in content {
        if let ToolCallContent::Content(c) = c {
            if let ContentBlock::Text(t) = &c.content {
                parts.push(t.text.clone());
            }
        }
    }
    if !parts.is_empty() {
        return parts.join("\n");
    }
    raw_output
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_default()
}

/// Name an update variant for logging.
pub(super) fn variant_name(u: &SessionUpdate) -> &'static str {
    match u {
        SessionUpdate::UserMessageChunk(_) => "user_message_chunk",
        SessionUpdate::AgentMessageChunk(_) => "agent_message_chunk",
        SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
        SessionUpdate::ToolCall(_) => "tool_call",
        SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
        SessionUpdate::Plan(_) => "plan",
        SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
        SessionUpdate::CurrentModeUpdate(_) => "current_mode_update",
        SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
        SessionUpdate::UsageUpdate(_) => "usage_update",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigSelectGroup, TextContent, ToolCallUpdateFields,
    };

    /// Every known option kind maps to itself; the enum is `#[non_exhaustive]`
    /// so anything else must fall back to the neutral variant rather than being
    /// presented to the user as a rejection.
    #[test]
    fn permission_kinds_map_to_ui_kinds() {
        assert_eq!(
            permission_kind(&PermissionOptionKind::AllowOnce),
            PermissionKind::AllowOnce
        );
        assert_eq!(
            permission_kind(&PermissionOptionKind::AllowAlways),
            PermissionKind::AllowAlways
        );
        assert_eq!(
            permission_kind(&PermissionOptionKind::RejectOnce),
            PermissionKind::RejectOnce
        );
        assert_eq!(
            permission_kind(&PermissionOptionKind::RejectAlways),
            PermissionKind::RejectAlways
        );
    }

    /// A pick becomes a `Selected` outcome; no pick must become `Cancelled`,
    /// which the spec requires once the turn is over.
    #[test]
    fn permission_outcome_reflects_the_choice() {
        match permission_outcome(Some("allow-once".into())) {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.0.as_ref(), "allow-once");
            }
            other => panic!("expected Selected, got {other:?}"),
        }
        assert_eq!(
            permission_outcome(None),
            RequestPermissionOutcome::Cancelled
        );
    }

    /// A diff carried in the tool call's content, with no `raw_input`, must
    /// still describe what the user is approving.
    #[test]
    fn permission_detail_renders_content_diff() {
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new().content(vec![ToolCallContent::Diff(
                Diff::new("/tmp/notes.txt", "new line\n").old_text("old line\n"),
            )]),
        );
        let detail = permission_detail(&tool_call);
        assert!(detail.contains("/tmp/notes.txt"), "missing path: {detail}");
        assert!(detail.contains("- old line"), "missing removal: {detail}");
        assert!(detail.contains("+ new line"), "missing addition: {detail}");
    }

    /// Falls back to the raw input when the agent sends no content, and is
    /// empty when neither is present.
    #[test]
    fn permission_detail_falls_back_to_raw_input() {
        let with_input = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new().raw_input(serde_json::json!({ "path": "/tmp/a" })),
        );
        assert!(permission_detail(&with_input).contains("/tmp/a"));

        let empty = ToolCallUpdate::new("call-1", ToolCallUpdateFields::new());
        assert_eq!(permission_detail(&empty), "");
    }

    /// Text content is shown even when `raw_input` is also present, since the
    /// agent chose it as the human-readable description.
    #[test]
    fn permission_detail_prefers_content_text() {
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new()
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new("Overwrite the config"),
                ))])
                .raw_input(serde_json::json!({ "path": "/tmp/a" })),
        );
        let detail = permission_detail(&tool_call);
        assert!(detail.contains("Overwrite the config"), "got: {detail}");
        assert!(!detail.contains("/tmp/a"), "got: {detail}");
    }

    /// Build an ungrouped select option from `(value, name, description)`.
    fn select_option(
        id: &str,
        category: Option<SessionConfigOptionCategory>,
        current: &str,
        values: Vec<(&str, &str, &str)>,
    ) -> SessionConfigOption {
        let values = values
            .into_iter()
            .map(|(value, name, description)| {
                SessionConfigSelectOption::new(value.to_string(), name).description(
                    if description.is_empty() {
                        None
                    } else {
                        Some(description.to_string())
                    },
                )
            })
            .collect::<Vec<_>>();
        let option = SessionConfigOption::select(
            id.to_string(),
            id,
            current.to_string(),
            SessionConfigSelectOptions::Ungrouped(values),
        );
        match category {
            Some(category) => option.category(category),
            None => option,
        }
    }

    /// The mode option is found by its spec category, and its values keep the
    /// order the agent gave — that order carries intent, so unlike models it
    /// must not be sorted. The values here carry descriptions, as the real
    /// agents' do, and they are simply not carried through.
    #[test]
    fn mode_catalog_keeps_the_agents_order() {
        let config_options = Some(vec![
            select_option(
                "model",
                Some(SessionConfigOptionCategory::Model),
                "zeta/z",
                vec![("zeta/z", "Z", "")],
            ),
            select_option(
                "mode",
                Some(SessionConfigOptionCategory::Mode),
                "ask",
                vec![
                    ("ask", "Ask", "Request permission before making any changes"),
                    ("build", "Build", "Write and modify code"),
                ],
            ),
        ]);

        let (id, current, modes) = mode_catalog(&config_options);

        assert_eq!(id.expect("mode option id").0.as_ref(), "mode");
        assert_eq!(current.as_deref(), Some("ask"));
        assert_eq!(
            modes.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["ask", "build"]
        );
        assert_eq!(
            modes.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["Ask", "Build"]
        );
    }

    /// An agent that omits the category is still understood: the option id the
    /// spec reserves for modes is the fallback.
    #[test]
    fn mode_catalog_falls_back_to_the_option_id() {
        let config_options = Some(vec![select_option(
            "mode",
            None,
            "plan",
            vec![("plan", "Plan", "")],
        )]);

        let (id, current, modes) = mode_catalog(&config_options);

        assert_eq!(id.expect("mode option id").0.as_ref(), "mode");
        assert_eq!(current.as_deref(), Some("plan"));
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].name, "Plan");
    }

    /// Having no mode option is normal (the agent has no modes), and a
    /// non-select option must not be mistaken for one.
    #[test]
    fn mode_catalog_is_empty_without_a_select_mode_option() {
        let no_modes: Option<Vec<SessionConfigOption>> = None;
        assert!(mode_catalog(&no_modes).0.is_none());

        let model_only = Some(vec![select_option(
            "model",
            Some(SessionConfigOptionCategory::Model),
            "a/x",
            vec![("a/x", "X", "")],
        )]);
        let (id, current, modes) = mode_catalog(&model_only);
        assert!(id.is_none(), "the model option is not a mode option");
        assert!(current.is_none());
        assert!(modes.is_empty());

        let boolean = Some(vec![
            SessionConfigOption::boolean("mode", "Mode", true)
                .category(SessionConfigOptionCategory::Mode),
        ]);
        assert!(
            mode_catalog(&boolean).0.is_none(),
            "a boolean option is not a select"
        );
    }

    /// The reasoning option is found by its spec category alone — agents name it
    /// differently, so unlike model and mode there is no id to fall back on —
    /// and the labels stay the agent's own.
    #[test]
    fn reasoning_catalog_reads_the_thought_level_option() {
        let config_options = Some(vec![
            select_option(
                "model",
                Some(SessionConfigOptionCategory::Model),
                "m",
                vec![("m", "M", "")],
            ),
            select_option(
                "thinking",
                Some(SessionConfigOptionCategory::ThoughtLevel),
                "max",
                vec![("off", "Off", ""), ("low", "low", ""), ("max", "max", "")],
            ),
        ]);

        let (id, current, efforts) = reasoning_catalog(&config_options);

        assert_eq!(id.expect("reasoning option id").0.as_ref(), "thinking");
        assert_eq!(current.as_deref(), Some("max"));
        assert_eq!(
            efforts
                .iter()
                .map(|effort| effort.value.as_str())
                .collect::<Vec<_>>(),
            ["off", "low", "max"]
        );
        assert_eq!(efforts[1].name, "low", "the agent's own label is kept");
    }

    /// Having no reasoning option is normal, and an option that merely shares a
    /// likely id (`effort`, opencode's) but is categorized otherwise must not be
    /// mistaken for one — the category, not the id, is what decides.
    #[test]
    fn reasoning_catalog_is_empty_without_a_thought_level_option() {
        let no_options: Option<Vec<SessionConfigOption>> = None;
        assert!(reasoning_catalog(&no_options).0.is_none());

        let wrong_category = Some(vec![
            select_option(
                "effort",
                Some(SessionConfigOptionCategory::Model),
                "a",
                vec![("a", "A", "")],
            ),
            SessionConfigOption::boolean("thinking", "Thinking", true)
                .category(SessionConfigOptionCategory::ThoughtLevel),
        ]);
        let (id, current, efforts) = reasoning_catalog(&wrong_category);
        assert!(id.is_none(), "neither option is a reasoning select");
        assert!(current.is_none());
        assert!(efforts.is_empty());
    }

    /// Guard for the shared lookup: grouped values flatten flat, the provider
    /// prefix is stripped from a name that repeats it, and the result stays
    /// sorted by provider then name.
    #[test]
    fn model_catalog_flattens_groups_and_sorts_by_provider() {
        let grouped = SessionConfigSelectOptions::Grouped(vec![
            SessionConfigSelectGroup::new(
                "zeta",
                "Zeta",
                vec![SessionConfigSelectOption::new("zeta/z2", "Zeta Z2")],
            ),
            SessionConfigSelectGroup::new(
                "alpha",
                "Alpha",
                vec![SessionConfigSelectOption::new("alpha/a2", "Model Two")],
            ),
        ]);
        let config_options = Some(vec![
            SessionConfigOption::select("model", "Model", "zeta/z2", grouped)
                .category(SessionConfigOptionCategory::Model),
            select_option(
                "mode",
                Some(SessionConfigOptionCategory::Mode),
                "ask",
                vec![("ask", "Ask", "")],
            ),
        ]);

        let (id, current, models) = model_catalog(&config_options);

        assert_eq!(id.expect("model option id").0.as_ref(), "model");
        assert_eq!(current.as_deref(), Some("zeta/z2"));
        assert_eq!(
            models
                .iter()
                .map(|m| m.provider.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"],
            "groups flatten into one list, sorted by provider not group order"
        );
        assert_eq!(models[1].name, "Zeta Z2");

        let prefixed = Some(vec![select_option(
            "model",
            Some(SessionConfigOptionCategory::Model),
            "alpha/a1",
            vec![
                ("alpha/a1", "Alpha/Model One", ""),
                ("zeta/z2", "Zeta Z2", ""),
            ],
        )]);
        let models = model_catalog(&prefixed).2;
        assert_eq!(
            models
                .iter()
                .map(|m| (m.provider.as_str(), m.name.as_str()))
                .collect::<Vec<_>>(),
            [("alpha", "Model One"), ("zeta", "Zeta Z2")],
            "a name repeating its provider is stripped, and provider sorts first"
        );
    }
}
