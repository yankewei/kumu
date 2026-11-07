//! The slice of the Codex `app-server` schema the probe reads.
//!
//! Hand-written, and pinned: the authoritative source for a given Codex version
//! is that version's own
//!
//! ```text
//! codex app-server generate-json-schema --out DIR
//! ```
//!
//! Every field here was checked against **codex-cli 0.153.4**. Re-run the
//! generator against a newer Codex before trusting these shapes; a snapshot of
//! that version's dump is vendored under `schema/codex-app-server/0.153.4/` so
//! the two can be compared without a Codex install.
//!
//! Unknown fields are ignored throughout (serde's default), which is what keeps
//! this slice viable across Codex releases.

use serde::Deserialize;

/// Result of `model/list`.
#[derive(Debug, Deserialize)]
pub(super) struct ModelListResult {
    #[serde(default)]
    pub data: Vec<Model>,
}

/// One entry of `model/list`. Only the fields the catalogue needs are modelled.
#[derive(Debug, Deserialize)]
pub(super) struct Model {
    /// The value to send back as `model` on a turn.
    pub model: String,
    /// The label to show; falls back to the id when absent.
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    /// Models Codex hides from its own picker.
    #[serde(default)]
    pub hidden: bool,
    /// The model the agent recommends; used as the active value until a
    /// session reports its own.
    #[serde(rename = "isDefault", default)]
    pub is_default: bool,
    /// The effort the agent uses for this model when none is overridden.
    #[serde(rename = "defaultReasoningEffort", default)]
    pub default_reasoning_effort: Option<String>,
    /// The efforts this model advertises; the set differs per model, so this is
    /// the catalogue the reasoning axis reads.
    #[serde(rename = "supportedReasoningEfforts", default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

/// One effort a model advertises. The wire also carries a `description` of it,
/// which kumu does not show — the value is the label — so it is left unparsed.
#[derive(Debug, Deserialize)]
pub(super) struct ReasoningEffortOption {
    /// The value to send back as `effort` on a turn.
    #[serde(rename = "reasoningEffort", default)]
    pub reasoning_effort: String,
}

/// Result of `skills/list`: the skills in reach of each working directory asked
/// about, one entry per directory.
#[derive(Debug, Deserialize)]
pub(super) struct SkillsListResult {
    #[serde(default)]
    pub data: Vec<SkillsListEntry>,
}

/// One working directory's skills. Its `cwd` and load `errors` are on the wire
/// and not modelled: kumu asks about one directory at a time, and a skill that
/// failed to load is simply not offered.
#[derive(Debug, Deserialize)]
pub(super) struct SkillsListEntry {
    #[serde(default)]
    pub skills: Vec<Skill>,
}

/// One skill from `skills/list`. `path` is the file that copy came from.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(super) struct Skill {
    pub name: String,
    pub path: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// `repo`, `user`, `system` or `admin`: whether the skill belongs to the
    /// working directory it was listed for or goes wherever Codex does.
    #[serde(default)]
    pub scope: String,
}

fn enabled_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `model/list` carries the value to send back and the label to show.
    #[test]
    fn model_list_reads_the_catalogue_fields() {
        let result: ModelListResult = serde_json::from_str(
            r#"{"data":[{"id":"gpt-5.6-sol","model":"gpt-5.6-sol",
                "displayName":"GPT-5.6-Sol","description":"d","hidden":false,
                "isDefault":true,"defaultReasoningEffort":"low",
                "supportedReasoningEfforts":[
                    {"reasoningEffort":"low","description":"Fast responses with lighter reasoning"},
                    {"reasoningEffort":"high","description":"Greater reasoning depth"}]}],
                "nextCursor":null}"#,
        )
        .unwrap();
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].model, "gpt-5.6-sol");
        assert_eq!(result.data[0].display_name, "GPT-5.6-Sol");
        assert!(result.data[0].is_default);
        assert_eq!(
            result.data[0].default_reasoning_effort.as_deref(),
            Some("low")
        );
        assert_eq!(
            result.data[0]
                .supported_reasoning_efforts
                .iter()
                .map(|effort| effort.reasoning_effort.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"],
            "the agent's own order is kept, and the description is not parsed"
        );
    }
}
