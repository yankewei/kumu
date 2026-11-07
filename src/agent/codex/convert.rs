//! Protocol -> app translation for the Codex backend. Everything the UI
//! consumes is a plain value produced here, so app-server shapes stop at this
//! boundary — the same rule the ACP backend keeps.

use std::collections::HashMap;

use crate::agent::event::{CommandInfo, ModelInfo, ReasoningInfo, SandboxInfo};

use super::protocol::{ModelListResult, Skill, SkillsListResult};

/// The provider label Codex models are filed under.
///
/// `model/list` reports no provider, and borrowing the thread's
/// `modelProvider` instead would make the catalogue regroup the moment a session
/// started, so the probe and a live session label alike.
const PROVIDER: &str = "openai";

/// The skills a session can invoke, from `skills/list`: the enabled ones, in
/// Codex's own order (repo, then user, then system), each name once.
///
/// A name repeated across scopes keeps its first — the most local — entry,
/// which is the one a list row can mean.
pub(super) fn skill_catalog(listed: SkillsListResult) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for skill in listed.data.into_iter().flat_map(|entry| entry.skills) {
        if skill.enabled
            && !skill.name.trim().is_empty()
            && !skills.iter().any(|seen| seen.name == skill.name)
        {
            skills.push(skill);
        }
    }
    skills
}

/// Slash-command names listed ahead of skills, with the hint each takes.
///
/// app-server has no slash commands. These are kumu's own names; a skill with
/// the same name is left out so the catalogue offers it once.
const BUILTIN_COMMANDS: &[(&str, Option<&str>)] = &[
    ("compact", None),
    ("review", Some("[审查说明]")),
    ("review-branch", Some("<分支>")),
    ("review-commit", Some("<commit sha>")),
    ("rename", Some("<名称>")),
    ("init", None),
];

/// The session's slash commands: the built-ins, then the skills.
///
/// A skill takes the rest of the line as free text, with nothing said about
/// what it should be. One named like a built-in is left out.
pub(super) fn command_catalog(skills: &[Skill]) -> Vec<CommandInfo> {
    let builtins = BUILTIN_COMMANDS.iter().map(|(name, hint)| CommandInfo {
        name: name.to_string(),
        hint: hint.map(str::to_string),
    });
    let skills = skills
        .iter()
        .filter(|skill| !BUILTIN_COMMANDS.iter().any(|(name, _)| *name == skill.name))
        .map(|skill| CommandInfo {
            name: skill.name.clone(),
            hint: None,
        });
    builtins.chain(skills).collect()
}

/// Build the catalogue catalog from `model/list`: its visible models, and the one
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
    // An agent that names no default still needs something for the axis.
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

/// The session-level list the catalogue offers for a model: its advertised efforts,
/// in the agent's order. Empty when the model is unknown or advertises none,
/// which is what keeps the axis off the toolbar.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    /// efforts yields none — the source of "no reasoning axis here".
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

    /// Shaped like a real `skills/list` reply: a disabled skill is not offered,
    /// and a name shadowed by a more local scope is offered once, as that scope's.
    #[test]
    fn the_skill_catalogue_is_the_enabled_skills_each_name_once() {
        let listed: SkillsListResult = serde_json::from_value(json!({
            "data": [{
                "cwd": "/repo",
                "errors": [],
                "skills": [
                    {"name": "code-simplifier", "path": "/repo/.agents/skills/code-simplifier/SKILL.md", "enabled": true, "scope": "repo"},
                    {"name": "pdf:pdf", "path": "/home/.codex/plugins/pdf/SKILL.md", "enabled": true, "scope": "user"},
                    {"name": "code-simplifier", "path": "/home/.agents/skills/code-simplifier/SKILL.md", "enabled": true, "scope": "user"},
                    {"name": "imagegen", "path": "/home/.codex/skills/.system/imagegen/SKILL.md", "enabled": false, "scope": "system"},
                ],
            }],
        }))
        .unwrap();
        let skills = skill_catalog(listed);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["code-simplifier", "pdf:pdf"]);
        assert_eq!(
            skills[0].path,
            "/repo/.agents/skills/code-simplifier/SKILL.md"
        );
    }

    /// The built-ins come first, and a skill named like one is not offered twice.
    #[test]
    fn the_command_catalogue_is_the_builtins_then_the_skills() {
        let skill = |name: &str| Skill {
            name: name.into(),
            path: format!("/skills/{name}/SKILL.md"),
            enabled: true,
            scope: "user".into(),
        };
        let names: Vec<String> = command_catalog(&[skill("pdf:pdf"), skill("review")])
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert_eq!(
            names,
            [
                "compact",
                "review",
                "review-branch",
                "review-commit",
                "rename",
                "init",
                "pdf:pdf"
            ]
        );
    }
}
