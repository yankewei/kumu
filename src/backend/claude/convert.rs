//! Protocol -> app translation. Claude Code exposes no discovery request to
//! map yet, so everything here is a fixed vocabulary: the values `claude`
//! accepts on its own flags, which the pickers can therefore offer before any
//! session exists.
//!
//! The labels are kumu's own where the concept is kumu's to name (as with
//! Codex's sandbox policies) and the agent's own where they are a scale the
//! user types and reads in Claude Code itself.

use crate::backend::event::{ModeInfo, ModelInfo, ReasoningInfo};

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

/// The permission modes kumu offers, from least to most permissive.
///
/// The values are `--permission-mode`'s own names, which is what lets a pick
/// ride back on that flag verbatim.
///
/// `default` (and `manual`, its alias) is deliberately absent, for the same
/// reason `default` is absent from the model list: it is not one mode among
/// others but the instruction to fall back to the session's configured
/// `defaultMode`. Sending it would override a setting the user made in Claude
/// Code's own configuration, which kumu must not do unless the user picked a
/// value here.
pub(super) fn mode_options() -> Vec<ModeInfo> {
    [
        ("plan", "计划"),
        ("acceptEdits", "自动接受编辑"),
        ("auto", "自动"),
        ("dontAsk", "不再询问"),
        ("bypassPermissions", "跳过权限检查"),
    ]
    .into_iter()
    .map(|(value, name)| ModeInfo {
        value: value.to_string(),
        name: name.to_string(),
    })
    .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The mode list is the permission modes, and `default`/`manual` are not
    /// among them: sending either would override the session's configured
    /// `defaultMode`.
    #[test]
    fn mode_options_omit_the_fallback_values() {
        let modes = mode_options();
        assert_eq!(
            modes.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            [
                "plan",
                "acceptEdits",
                "auto",
                "dontAsk",
                "bypassPermissions"
            ]
        );
        assert!(
            modes.iter().all(|m| m.name != m.value),
            "each mode is labelled for display, not by its wire name"
        );
        assert!(
            !modes
                .iter()
                .any(|m| m.value == "default" || m.value == "manual"),
            "the fallback sentinel must not be selectable"
        );
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
}
