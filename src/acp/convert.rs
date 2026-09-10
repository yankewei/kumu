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

use super::event::{ModelInfo, PermissionKind};

/// Extract the session's model config option: its id (for
/// `session/set_config_option`), the value the agent reports as active, and
/// the selectable models, grouped flat.
pub(super) fn model_catalog(
    config_options: &Option<Vec<SessionConfigOption>>,
) -> (Option<SessionConfigId>, Option<String>, Vec<ModelInfo>) {
    let mut option_id = None;
    let mut current = None;
    let mut models: Vec<ModelInfo> = Vec::new();
    if let Some(options) = config_options {
        for option in options {
            let is_model = matches!(&option.category, Some(SessionConfigOptionCategory::Model))
                || option.id.0.as_ref() == "model";
            if !is_model {
                continue;
            }
            if let SessionConfigKind::Select(select) = &option.kind {
                option_id = Some(option.id.clone());
                current = Some(select.current_value.0.as_ref().to_string());
                let mut add = |value: &SessionConfigSelectOption| {
                    let raw = value.value.0.as_ref();
                    let provider = raw.split('/').next().unwrap_or(raw).to_string();
                    models.push(ModelInfo {
                        value: raw.to_string(),
                        name: strip_provider_prefix(&value.name, &provider),
                        provider,
                    });
                };
                match &select.options {
                    SessionConfigSelectOptions::Ungrouped(values) => {
                        for v in values {
                            add(v);
                        }
                    }
                    SessionConfigSelectOptions::Grouped(groups) => {
                        for group in groups {
                            for v in &group.options {
                                add(v);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
    (option_id, current, models)
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
        SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
        SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
        SessionUpdate::UsageUpdate(_) => "usage_update",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{TextContent, ToolCallUpdateFields};

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
}
