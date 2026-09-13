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

use super::KumuApp;
use crate::backend;
use crate::db::Session;
use crate::ui::AgentSessionOptions;
use crate::ui::SessionOptions;
use crate::ui::widgets::{ChipTrigger, inherited_label, select_row};

/// The effort in force: the user's stored pick when the agent still offers it,
/// else the value the agent reports.
///
/// The membership test is what lets the UI stay protocol-blind: a Codex pick
/// made for a model that has since changed is simply not in the list any more,
/// so it drops out here and the agent's reported value takes over.
fn effective_value<'a>(options: &'a SessionOptions, stored: Option<&'a str>) -> Option<&'a str> {
    stored
        .filter(|value| {
            options
                .reasoning
                .iter()
                .any(|effort| effort.value == *value)
        })
        .or(options.current_reasoning.as_deref())
}

/// The label for the reasoning chip: the effort in force, resolved to the
/// agent's own name for it. Falls back to saying nothing is overridden, so an
/// agent offering the axis always has something to show.
///
/// A value the agent no longer lists still shows as itself rather than going
/// blank.
fn reasoning_label(options: &SessionOptions, stored: Option<&str>, agent: &str) -> String {
    match effective_value(options, stored) {
        Some(value) => options
            .reasoning
            .iter()
            .find(|effort| effort.value == value)
            .map(|effort| effort.name.clone())
            .unwrap_or_else(|| value.to_string()),
        None => inherited_label(agent),
    }
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
        let current = effective_value(&options, session.reasoning.as_deref()).map(str::to_string);
        let efforts = options.reasoning;

        let weak = cx.entity().downgrade();
        let open = self.reasoning_picker_open;
        let (radius, muted, muted_fg, border, accent, accent_fg, background) = {
            let theme = cx.theme();
            (
                theme.radius,
                theme.muted,
                theme.muted_foreground,
                theme.border,
                theme.sidebar_accent,
                theme.sidebar_accent_foreground,
                theme.background,
            )
        };

        // A brain glyph leads the chip, the way the model chip leads with the
        // agent's logo; the effort name is the rest of the affordance.
        let chip = ChipTrigger {
            element: h_flex()
                .id("reasoning-picker-chip")
                .h(px(24.))
                .px_1p5()
                .gap_1p5()
                .rounded(radius)
                .cursor_pointer()
                .text_color(muted_fg)
                .when(open, |el| el.bg(muted.opacity(0.6)))
                .when(!open, |el| el.hover(move |s| s.bg(muted.opacity(0.6))))
                .child(
                    Icon::empty()
                        .path("icons/brain.svg")
                        .with_size(px(13.))
                        .text_color(muted_fg),
                )
                .child(div().max_w(px(160.)).text_xs().truncate().child(label)),
        };

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
                            app.reasoning_picker_open = open;
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = v_flex().gap_0p5().p_1p5();
                    for effort in &efforts {
                        let value = effort.value.clone();
                        let selected = current.as_deref() == Some(effort.value.as_str());
                        let weak = weak.clone();
                        // The agent's own name for the effort, nothing more: its
                        // description is longer than the choice is worth.
                        list = list.child(select_row(
                            &effort.name,
                            None,
                            selected,
                            radius,
                            muted,
                            muted_fg,
                            accent,
                            accent_fg,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.pick_reasoning(value.clone(), cx);
                                    app.reasoning_picker_open = false;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    // Content-driven with a cap, like the mode and sandbox menus.
                    v_flex()
                        .max_w(px(320.))
                        .max_h(px(320.))
                        .rounded(radius)
                        .border_1()
                        .border_color(border)
                        .bg(background)
                        .shadow_lg()
                        .overflow_hidden()
                        .child(
                            div()
                                .id("reasoning-picker-list")
                                .overflow_y_scroll()
                                .child(list),
                        )
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

    use super::{effective_value, reasoning_label};
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

    /// The stored pick wins while the agent still offers it; once it does not
    /// (a Codex model change, say) the agent's reported value takes over.
    #[test]
    fn the_stored_pick_only_stands_while_it_is_offered() {
        let options = session_options(
            vec![effort("low", "Low"), effort("high", "High")],
            Some("low"),
        );
        assert_eq!(effective_value(&options, Some("high")), Some("high"));
        assert_eq!(
            effective_value(&options, Some("gone")),
            Some("low"),
            "a pick the agent no longer lists falls back to its report"
        );
        assert_eq!(effective_value(&options, None), Some("low"));
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
