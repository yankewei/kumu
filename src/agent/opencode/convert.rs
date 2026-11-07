//! opencode v2 -> app translation: the catalogue a probe reads.
//!
//! Where protocol shapes stop. The payloads are read as JSON as they arrive
//! rather than pinned as types: the server describes none of its responses (see
//! `schema/opencode-v2/2.0.16/README.md`), so the fields read here are exactly
//! the ones the captured traffic shows, and a missing one degrades to nothing
//! rather than failing the whole read.

use serde_json::Value;

use crate::agent::event::{CommandInfo, Event, ModeInfo, ModelInfo, ReasoningInfo};
use crate::agent::session::SessionOptionsBuilder;

/// One model as `GET /api/model` lists it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
    /// The model's own reasoning levels (`variants`), in its order.
    pub variants: Vec<String>,
}

impl ModelEntry {
    /// The value kumu stores and sends back: `provider/id`. The id
    /// may contain `/` itself (`deepseek/deepseek-v4.1-flash`), so a value is
    /// split at its first `/` only — which is also the shape the ACP backend
    /// stored, so a session's saved pick carries over.
    pub fn value(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// The models a location offers. A model the server marks `enabled: false` is
/// not offered.
pub(super) fn model_entries(response: &Value) -> Vec<ModelEntry> {
    let Some(models) = response.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|model| model.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|model| {
            let text = |key: &str| model.get(key).and_then(Value::as_str).map(str::to_string);
            let provider = text("providerID")?;
            let id = text("id")?;
            Some(ModelEntry {
                name: text("name").unwrap_or_else(|| id.clone()),
                variants: model
                    .get("variants")
                    .and_then(Value::as_array)
                    .map(|variants| {
                        variants
                            .iter()
                            .filter_map(|v| v.get("id").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                provider,
                id,
            })
        })
        .collect()
}

/// A `Model.Ref` (`{id, providerID, variant?}`) as the stored value and its
/// variant.
pub(super) fn model_ref(value: &Value) -> Option<(String, Option<String>)> {
    let provider = value.get("providerID")?.as_str()?;
    let id = value.get("id")?.as_str()?;
    let variant = value
        .get("variant")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((format!("{provider}/{id}"), variant))
}

/// The modes a session can switch between: the primary agents the user can
/// pick (`build`, `plan`). Subagents and the hidden ones opencode runs itself
/// (title, summary, compaction) are not modes.
pub(super) fn mode_catalog(response: &Value) -> Vec<ModeInfo> {
    let Some(agents) = response.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    agents
        .iter()
        .filter(|agent| agent.get("mode").and_then(Value::as_str) == Some("primary"))
        .filter(|agent| agent.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|agent| {
            let id = agent.get("id")?.as_str()?.to_string();
            let name = agent
                .get("name")
                .and_then(Value::as_str)
                .map_or_else(|| id.clone(), str::to_string);
            Some(ModeInfo { value: id, name })
        })
        .collect()
}

/// The commands a session accepts as `/name`, in the server's order.
pub(super) fn command_catalog(response: &Value) -> Vec<CommandInfo> {
    response
        .get("data")
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| command.get("name")?.as_str())
                .map(|name| CommandInfo {
                    name: name.to_string(),
                    hint: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a probe reports: the catalogue plus the picks in force, from which
/// the options event is built.
#[derive(Clone, Debug, Default)]
pub(super) struct Catalog {
    pub models: Vec<ModelEntry>,
    pub modes: Vec<ModeInfo>,
    /// The model in force, as `provider/id`.
    pub model: Option<String>,
    /// Its reasoning level; `None` is the model's own default.
    pub variant: Option<String>,
    pub mode: Option<String>,
}

impl Catalog {
    fn current(&self) -> Option<&ModelEntry> {
        let (provider, id) = self.model.as_deref()?.split_once('/')?;
        self.models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == id)
    }

    /// The session's options. Reasoning is the model in force's own list, so
    /// switching model changes what the effort axis offers.
    pub fn options(&self, agent: &'static str) -> Event {
        let models = self
            .models
            .iter()
            .map(|entry| ModelInfo {
                value: entry.value(),
                name: entry.name.clone(),
                provider: entry.provider.clone(),
            })
            .collect();
        let reasoning = self
            .current()
            .map(|entry| {
                entry
                    .variants
                    .iter()
                    .map(|variant| ReasoningInfo {
                        value: variant.clone(),
                        name: variant.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        // No access or sandbox axis: opencode's permission rules are its own
        // configuration, which kumu does not override.
        SessionOptionsBuilder::new(agent)
            .models(models, self.model.clone())
            .modes(self.modes.clone(), self.mode.clone())
            .reasoning(reasoning, self.variant.clone())
            .build()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn captured(name: &str) -> Value {
        let text = match name {
            "model-list" => include_str!("../../../schema/opencode-v2/2.0.16/model-list.json"),
            "agent-list" => include_str!("../../../schema/opencode-v2/2.0.16/agent-list.json"),
            "command-list" => include_str!("../../../schema/opencode-v2/2.0.16/command-list.json"),
            _ => unreachable!("{name}"),
        };
        serde_json::from_str(text).unwrap()
    }

    /// The catalogue: models as `provider/id` with their own levels, the
    /// primary visible agents as modes, and the commands.
    #[test]
    fn the_catalogue_reads_models_modes_and_commands() {
        let models = model_entries(&captured("model-list"));
        let haiku = models.iter().find(|m| m.id.contains("haiku")).unwrap();
        assert_eq!(
            haiku.value(),
            "commandcode-anthropic/claude-haiku-4-5-20251001"
        );
        assert_eq!(haiku.variants, ["high", "max"]);

        let modes: Vec<String> = mode_catalog(&captured("agent-list"))
            .into_iter()
            .map(|m| m.value)
            .collect();
        assert_eq!(modes, ["build", "plan"]);

        let commands: Vec<String> = command_catalog(&captured("command-list"))
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(commands, ["init", "review"]);
    }

    /// Reasoning follows the model in force.
    #[test]
    fn reasoning_is_the_current_models_own() {
        let catalog = Catalog {
            models: model_entries(&captured("model-list")),
            model: Some("commandcode-anthropic/claude-haiku-4-5-20251001".into()),
            ..Default::default()
        };
        let Event::SessionOptions { reasoning, .. } = catalog.options("opencode") else {
            unreachable!()
        };
        let levels: Vec<&str> = reasoning.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(levels, ["high", "max"]);
    }
}
