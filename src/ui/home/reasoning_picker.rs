//! Composer reasoning picker: the session's reasoning effort, as an
//! agent-reported select.
//!
//! One axis for every agent, whatever its protocol: ACP exposes it as a
//! session-wide config option (`thought_level`), Codex reduces its per-model
//! sets to the same session-level list before it reaches here. So nothing below
//! the chip knows or cares which agent is talking.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::popover::Popover;

use super::{KumuApp, PickerAxis};
use crate::backend;
use crate::db::Session;
use crate::ui::AgentSessionOptions;
use crate::ui::SessionOptions;
use crate::ui::widgets::{
    PickerChrome, choice_label, in_force, picker_chip, picker_list, picker_menu, select_row,
};

/// The label for the reasoning chip: the effort in force, resolved to the
/// agent's own name for it. Falls back to saying nothing is overridden, so an
/// agent offering the axis always has something to show.
fn reasoning_label(options: &SessionOptions, stored: Option<&str>, agent: &str) -> String {
    let value = in_force(
        &options.reasoning,
        stored,
        options.current_reasoning.as_deref(),
    );
    choice_label(&options.reasoning, value, agent)
}

impl KumuApp {
    /// Switch the session's reasoning effort: persist it, then apply it over the
    /// session's effort. A session with no live connection just stores the pick,
    /// which is re-applied when one is opened.
    fn pick_reasoning(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        self.store
            .update_session_reasoning(session_id, Some(&value))
            .ok();
        self.refresh();
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(backend::Cmd::SetReasoning(value));
        }
        cx.notify();
    }

    /// The composer's reasoning picker: a text chip led by a brain glyph, which
    /// opens the efforts the agent offers.
    ///
    /// Renders nothing for an agent without the axis — and nothing before a
    /// probe or session has reported one.
    pub(super) fn reasoning_picker(
        &self,
        session: &Session,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let options = cx
            .try_global::<AgentSessionOptions>()
            .and_then(|catalogs| catalogs.0.get(session.agent.as_str()))
            .cloned()?;
        if options.reasoning.is_empty() {
            return None;
        }
        let label = reasoning_label(
            &options,
            session.reasoning.as_deref(),
            &self.agent_name(&session.agent),
        );
        let current = in_force(
            &options.reasoning,
            session.reasoning.as_deref(),
            options.current_reasoning.as_deref(),
        )
        .map(str::to_string);
        let efforts = options.reasoning;

        let weak = cx.entity().downgrade();
        let chrome = PickerChrome::new(cx.theme());
        let open = self.open_picker == Some(PickerAxis::Reasoning);

        // A brain glyph leads the chip, the way the model chip leads with the
        // agent's logo; the effort name is the rest of the affordance.
        let chip = picker_chip(
            "reasoning-picker-chip",
            &chrome,
            label,
            Some(
                Icon::empty()
                    .path("icons/brain.svg")
                    .with_size(px(13.))
                    .text_color(chrome.muted_fg)
                    .into_any_element(),
            ),
            open,
        );

        Some(
            Popover::new("reasoning-picker")
                .appearance(false)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let weak = weak.clone();
                    move |open, _window, cx| {
                        let open = *open;
                        let _ = weak.update(cx, |app, cx| {
                            app.set_picker_open(PickerAxis::Reasoning, open);
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = picker_list();
                    for effort in &efforts {
                        let value = effort.value.clone();
                        let selected = current.as_deref() == Some(effort.value.as_str());
                        let weak = weak.clone();
                        // The agent's own name for the effort, nothing more: its
                        // description is longer than the choice is worth.
                        list = list.child(select_row(
                            &chrome,
                            &effort.name,
                            None,
                            selected,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.pick_reasoning(value.clone(), cx);
                                    app.open_picker = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    picker_menu("reasoning-picker-list", &chrome, list)
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::reasoning_label;
    use crate::backend::ReasoningInfo;
    use crate::ui::SessionOptions;
    use crate::ui::widgets::inherited_label;

    fn effort(value: &str, name: &str) -> ReasoningInfo {
        ReasoningInfo {
            value: value.into(),
            name: name.into(),
        }
    }

    fn session_options(
        reasoning: Vec<ReasoningInfo>,
        current_reasoning: Option<&str>,
    ) -> SessionOptions {
        SessionOptions {
            reasoning,
            current_reasoning: current_reasoning.map(str::to_owned),
            ..Default::default()
        }
    }

    /// The label is the agent's own name for the effort; a pick it no longer
    /// lists falls back to the reported value, and a reported value it no longer
    /// lists still shows as itself rather than going blank.
    #[test]
    fn reasoning_label_resolves_through_the_agents_names() {
        let options = session_options(
            vec![effort("low", "low"), effort("high", "High")],
            Some("low"),
        );
        assert_eq!(
            reasoning_label(&options, Some("high"), "Claude Code"),
            "High"
        );
        assert_eq!(reasoning_label(&options, None, "Claude Code"), "low");
        assert_eq!(
            reasoning_label(&options, Some("gone"), "Claude Code"),
            "low",
            "a dropped pick yields to the agent's report"
        );

        let stale = session_options(vec![effort("low", "Low")], Some("ultra"));
        assert_eq!(reasoning_label(&stale, None, "Claude Code"), "ultra");
    }

    /// With nothing to report at all the chip still says something, rather than
    /// going blank on an agent that does offer the axis.
    #[test]
    fn reasoning_label_says_so_when_nothing_is_in_force() {
        let options = session_options(vec![effort("low", "Low")], None);
        assert_eq!(
            reasoning_label(&options, None, "Claude Code"),
            inherited_label("Claude Code")
        );
    }
}
