//! Composer access picker: how much the session may do before it has to ask,
//! as an agent-reported select.
//!
//! A different axis from the work mode chip beside it — that one says what the
//! agent is doing (`build` / `plan`), this one says what it may do unattended.
//! Claude Code is the agent that has both, and it reports them from a single
//! value: its permission mode. So a pick here is stored and sent the same way a
//! work-mode pick is (`KumuApp::pick_mode`), and the two chips are two views of
//! the one value in force.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::popover::Popover;

use super::KumuApp;
use crate::db::Session;
use crate::ui::AgentSessionOptions;
use crate::ui::SessionOptions;
use crate::ui::widgets::{ChipTrigger, inherited_label, select_row};

/// The label for the access chip: the user's stored pick while it is one of the
/// offers, else the policy the agent reports as in force, resolved to a name
/// through the catalogue.
///
/// The membership test matters because the session stores one value for both
/// axes: a stored `plan` is a work mode, not an access policy, so it must drop
/// out here rather than be shown as a policy kumu never offered. A policy the
/// catalogue no longer lists still shows as itself. An agent with no access axis
/// has no chip at all.
fn access_label(
    options: Option<&SessionOptions>,
    stored: Option<&str>,
    agent: &str,
) -> Option<String> {
    let options = options?;
    if options.access.is_empty() {
        return None;
    }
    let value = stored
        .filter(|value| options.access.iter().any(|policy| policy.value == *value))
        .or(options.current_access.as_deref());
    Some(match value {
        Some(value) => options
            .access
            .iter()
            .find(|policy| policy.value == value)
            .map(|policy| policy.name.clone())
            .unwrap_or_else(|| value.to_string()),
        None => inherited_label(agent),
    })
}

impl KumuApp {
    /// The composer's access picker: a text chip showing the policy in force,
    /// which opens the policies the agent offers.
    ///
    /// Renders nothing for an agent without the axis — and nothing before a
    /// probe or session has reported one.
    pub(super) fn access_picker(
        &self,
        session: &Session,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let options = cx
            .try_global::<AgentSessionOptions>()
            .and_then(|catalogs| catalogs.0.get(session.agent.as_str()))
            .cloned()?;
        let label = access_label(
            Some(&options),
            session.mode.as_deref(),
            &self.agent_name(&session.agent),
        )?;
        let policies = options.access;

        let weak = cx.entity().downgrade();
        let open = self.access_picker_open;
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

        // Plain text, like the other chips: the policy name is the whole
        // affordance.
        let chip = ChipTrigger {
            element: h_flex()
                .id("access-picker-chip")
                .h(px(24.))
                .px_1p5()
                .rounded(radius)
                .cursor_pointer()
                .text_color(muted_fg)
                .when(open, |el| el.bg(muted.opacity(0.6)))
                .when(!open, |el| el.hover(move |s| s.bg(muted.opacity(0.6))))
                .child(div().max_w(px(160.)).text_xs().truncate().child(label)),
        };

        Some(
            Popover::new("access-picker")
                .appearance(false)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let weak = weak.clone();
                    move |open, _window, cx| {
                        let open = *open;
                        let _ = weak.update(cx, |app, cx| {
                            app.access_picker_open = open;
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = v_flex().gap_0p5().p_1p5();
                    for policy in &policies {
                        let value = policy.value.clone();
                        let selected =
                            options.current_access.as_deref() == Some(policy.value.as_str());
                        let weak = weak.clone();
                        // Name only, like the other pickers: what each policy
                        // means belongs in the docs, not in a menu row.
                        list = list.child(select_row(
                            &policy.name,
                            None,
                            selected,
                            radius,
                            muted,
                            muted_fg,
                            accent,
                            accent_fg,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    // Same store, same command as the work-mode
                                    // chip: the agent reports one value for both.
                                    app.pick_mode(value.clone(), cx);
                                    app.access_picker_open = false;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    // Sized to the names, like the mode picker: content-driven
                    // with a cap, so a list of bare labels reserves no room for a
                    // description it has none of.
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
                                .id("access-picker-list")
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

    use crate::ui::widgets::inherited_label;

    use super::access_label;
    use crate::backend::AccessInfo;
    use crate::ui::SessionOptions;

    fn policy(value: &str, name: &str) -> AccessInfo {
        AccessInfo {
            value: value.into(),
            name: name.into(),
        }
    }

    fn options(current_access: Option<&str>) -> SessionOptions {
        SessionOptions {
            access: vec![
                policy("default", "监督"),
                policy("acceptEdits", "自动接受编辑"),
            ],
            current_access: current_access.map(str::to_owned),
            ..Default::default()
        }
    }

    /// The stored pick wins while it is one of the offers, and either resolves
    /// through the catalogue to the agent's own label.
    #[test]
    fn access_label_prefers_the_stored_pick() {
        let options = options(Some("default"));
        assert_eq!(
            access_label(Some(&options), Some("acceptEdits"), "Claude Code").as_deref(),
            Some("自动接受编辑")
        );
        assert_eq!(
            access_label(Some(&options), None, "Claude Code").as_deref(),
            Some("监督")
        );
    }

    /// One value is stored for both axes, so a stored work mode must not be read
    /// as an access policy: it drops out and the report decides instead.
    #[test]
    fn a_stored_work_mode_is_not_an_access_policy() {
        assert_eq!(
            access_label(Some(&options(None)), Some("plan"), "Claude Code").as_deref(),
            Some(inherited_label("Claude Code").as_str()),
            "planning anyway leaves no policy in force to name"
        );
        assert_eq!(
            access_label(Some(&options(Some("auto"))), Some("plan"), "Claude Code").as_deref(),
            Some("auto"),
            "the report is what says a policy is in force"
        );
    }

    /// An agent with no access axis has no chip.
    #[test]
    fn access_label_is_none_without_the_axis() {
        assert_eq!(access_label(None, Some("default"), "Claude Code"), None);
        assert_eq!(
            access_label(Some(&SessionOptions::default()), None, "Claude Code"),
            None
        );
    }
}
