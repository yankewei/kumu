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

use super::{KumuApp, PickerAxis};
use crate::db::Session;
use crate::ui::AgentSessionOptions;
use crate::ui::SessionOptions;
use crate::ui::widgets::{
    PickerChrome, axis_label, in_force, picker_chip, picker_list, picker_menu, select_row,
};

/// The label for the access chip: the value in force on the access axis,
/// resolved to the agent's own name for it, and no chip at all when the agent
/// reports no policies.
///
/// On Claude Code this axis shares its stored value with the work mode — one
/// permission mode, two chips — so a stored `plan` is a work mode, not a policy,
/// and drops out here rather than being shown as a policy kumu never offered.
fn access_label(
    options: Option<&SessionOptions>,
    stored: Option<&str>,
    agent: &str,
) -> Option<String> {
    let options = options?;
    let value = in_force(&options.access, stored, options.current_access.as_deref());
    axis_label(&options.access, value, agent)
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
        let chrome = PickerChrome::new(cx.theme());
        let open = self.open_picker == Some(PickerAxis::Access);

        // Plain text, like the other chips: the policy name is the whole
        // affordance.
        let chip = picker_chip("access-picker-chip", &chrome, label, None, open);

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
                            app.set_picker_open(PickerAxis::Access, open);
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = picker_list();
                    for policy in &policies {
                        let value = policy.value.clone();
                        let selected =
                            options.current_access.as_deref() == Some(policy.value.as_str());
                        let weak = weak.clone();
                        // Name only, like the other pickers: what each policy
                        // means belongs in the docs, not in a menu row.
                        list = list.child(select_row(
                            &chrome,
                            &policy.name,
                            None,
                            selected,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    // Same store, same command as the work-mode
                                    // chip: the agent reports one value for both.
                                    app.pick_mode(value.clone(), cx);
                                    app.open_picker = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    picker_menu("access-picker-list", &chrome, list)
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
    use crate::agent::AccessInfo;
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
