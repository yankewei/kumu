//! Protocol -> app translation. Protocol shapes stop here.
//!
//! Everything Pi reports arrives in the `data` object of a `response`, so the
//! functions below take that object rather than the whole envelope.
//!
//! The labels are the agent's own where they are a scale the user reads back in
//! Pi itself (thinking levels), and derived from Pi's payload where the agent
//! reports them (models and their providers). Nothing here is a fixed
//! vocabulary: unlike Claude Code, Pi advertises its catalogue over the
//! protocol, so the menu is whatever this machine is configured for.

use serde_json::Value;

use crate::agent::event::{CommandInfo, ModelInfo, ReasoningInfo};

/// Provider and model id are joined with `/` because that is the form Pi's own
/// `--model` accepts, and therefore the form kumu sends back.
const SLUG_SEPARATOR: char = '/';

/// One model, as the session reports it.
struct ReportedModel {
    provider: String,
    id: String,
    name: String,
}

/// The models this machine can reach, grouped by provider so a list of a
/// hundred-odd entries reads as blocks rather than as one run.
///
/// The provider is taken from the payload, not parsed back out of the value:
/// Pi names a provider per model, and a provider id may itself contain no
/// separator, which is exactly why the value is built rather than assumed.
pub(super) fn model_catalog(data: &Value) -> Vec<ModelInfo> {
    let mut models: Vec<ReportedModel> = data
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    let field = |key: &str| {
                        model
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                    };
                    Some(ReportedModel {
                        provider: field("provider")?.to_string(),
                        id: field("id")?.to_string(),
                        // The label Pi shows, falling back to the id: naming the
                        // model is more use than repeating the slug.
                        name: field("name").unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    models.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });

    models
        .into_iter()
        .map(|model| ModelInfo {
            value: format!("{}{SLUG_SEPARATOR}{}", model.provider, model.id),
            name: if model.name.is_empty() {
                model.id
            } else {
                model.name
            },
            provider: model.provider,
        })
        .collect()
}

/// The model in force, as the `provider/model` value a session selects on.
pub(super) fn current_model(data: &Value) -> Option<String> {
    let model = data.get("model")?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let id = model.get("id").and_then(Value::as_str)?;
    (!provider.is_empty() && !id.is_empty()).then(|| format!("{provider}{SLUG_SEPARATOR}{id}"))
}

/// The thinking levels the selected model honours.
///
/// Asked for the *current* model rather than read off the catalogue: the levels
/// live on each model, and Pi is the one that knows which of them the selected
/// one supports. `off` is among them — Pi lists it even for a model with no
/// reasoning support, which is what makes the list a complete answer for the
/// axis instead of needing a value added back.
pub(super) fn reasoning_catalog(data: &Value) -> Vec<ReasoningInfo> {
    data.get("levels")
        .and_then(Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|level| !level.is_empty())
                .map(|level| ReasoningInfo {
                    // A scale the user passes back and reads in Pi's own header,
                    // so the value is its label rather than a translation.
                    value: level.to_string(),
                    name: level.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The commands this Pi will run, from a `get_commands` reply.
///
/// Pi's own documentation says these are exactly the ones a client may invoke
/// over RPC — by sending `/name` as a prompt — and deliberately excludes the
/// commands its TUI handles itself. Its `source` (`extension` / `prompt` /
/// `skill`) is not carried: the name already says which namespace it is in
/// (`skill:` is part of it), and no row in a kumu menu names a source.
///
/// Pi reports a description for each and kumu carries none: a list row is the
/// name and nothing more, so it is dropped here rather than at render time.
pub(super) fn command_catalog(data: &Value) -> Vec<CommandInfo> {
    data.get("commands")
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| command.get("name").and_then(Value::as_str))
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| CommandInfo {
                    name: name.to_string(),
                    // A command takes the rest of the line, but Pi says nothing
                    // about what that should be.
                    hint: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The thinking level in force.
pub(super) fn current_reasoning(data: &Value) -> Option<String> {
    let level = data.get("thinkingLevel").and_then(Value::as_str)?;
    let level = level.trim();
    (!level.is_empty()).then(|| level.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The command catalogue is what Pi says a client may invoke — extension
    /// commands, prompt templates and skills alike — in Pi's own order, with the
    /// `skill:` prefix that makes a skill addressable part of the name. An entry
    /// without a name is not an offer, and nothing here invents a hint.
    #[test]
    fn the_command_catalogue_is_the_sessions_own() {
        let commands = command_catalog(&json!({
            "commands": [
                {"name": "session-name", "description": "Set or clear session name", "source": "extension"},
                {"name": "fix-tests", "description": "Fix failing tests", "source": "prompt"},
                {"name": "skill:brave-search", "description": "Web search", "source": "skill"},
                {"description": "nameless", "source": "prompt"},
            ]
        }));
        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["session-name", "fix-tests", "skill:brave-search"]
        );
        assert!(
            commands.iter().all(|command| command.hint.is_none()),
            "pi describes no input, so no row claims one"
        );
    }

    /// The catalogue is built from what the session reports, as `provider/id`
    /// values, and reads grouped by provider.
    #[test]
    fn the_catalogue_is_the_sessions_own_and_reads_grouped() {
        let models = model_catalog(&json!({
            "models": [
                {"id": "sonnet-5", "name": "Sonnet 5", "provider": "anthropic"},
                {"id": "gpt-5.5", "name": "GPT-5.5", "provider": "openai"},
                {"id": "haiku-4", "name": "Haiku 4", "provider": "anthropic"},
            ]
        }));
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["anthropic/haiku-4", "anthropic/sonnet-5", "openai/gpt-5.5"],
            "grouped by provider, then by label"
        );
        assert_eq!(models[0].provider, "anthropic");
        assert_eq!(models[0].name, "Haiku 4");
    }

    /// A model whose provider or id is missing cannot be turned into a value
    /// `set_model` would accept, so it is left out rather than offered as an
    /// unusable entry.
    #[test]
    fn entries_without_a_provider_or_id_are_left_out() {
        let models = model_catalog(&json!({
            "models": [
                {"id": "gpt-5.5", "provider": "openai"},
                {"name": "No provider", "id": "x"},
                {"id": "", "provider": "openai"},
                {"id": "no-name", "provider": "openai"},
            ]
        }));
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["openai/gpt-5.5", "openai/no-name"]
        );
        // A model Pi names nothing still gets a label.
        assert_eq!(models[1].name, "no-name");
        assert!(model_catalog(&json!({})).is_empty());
    }

    /// The model in force is the pair Pi reports, joined the way `set_model`
    /// takes it back.
    #[test]
    fn the_current_model_is_the_reported_pair() {
        assert_eq!(
            current_model(&json!({"model": {"provider": "opencode-go", "id": "kimi-k3"}}))
                .as_deref(),
            Some("opencode-go/kimi-k3")
        );
        assert_eq!(current_model(&json!({"model": null})), None);
        assert_eq!(current_model(&json!({"model": {"id": "x"}})), None);
    }

    /// The levels come from the current model's own list, which includes `off`.
    #[test]
    fn thinking_levels_come_from_the_model_in_force() {
        let levels = reasoning_catalog(&json!({
            "levels": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        }));
        assert_eq!(
            levels.iter().map(|l| l.value.as_str()).collect::<Vec<_>>(),
            ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert!(
            levels.iter().all(|level| level.name == level.value),
            "the value is the label, as Pi names its own scale"
        );
        assert_eq!(
            current_reasoning(&json!({"thinkingLevel": "high"})).as_deref(),
            Some("high")
        );
        assert_eq!(current_reasoning(&json!({"thinkingLevel": "  "})), None);
        assert!(reasoning_catalog(&json!({})).is_empty());
    }
}
