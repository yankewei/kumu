//! Composer sandbox picker: the session's sandbox policy, as an agent-reported
//! select.
//!
//! Separate from the mode picker on purpose: modes and the sandbox are different
//! axes, and an agent may have either, both, or neither.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::popover::Popover;

use super::{KumuApp, PickerAxis};
use crate::app::Axis;
use crate::app::discovery::SessionOptions;
use crate::db::Session;
use crate::ui::widgets::{
    PickerChrome, axis_label, picker_chip, picker_list, picker_menu, select_row,
};

/// The label for the sandbox chip, or no chip at all when the agent reports no
/// policies.
///
/// Unlike the mode and access chips there is no membership test: this axis has
/// the session's stored value to itself, so a stored pick is always the pick in
/// force — and one the catalogue no longer lists still shows as itself rather
/// than leaving the chip blank.
fn sandbox_label(
    options: Option<&SessionOptions>,
    stored: Option<&str>,
    agent: &str,
) -> Option<String> {
    let options = options?;
    let value = stored.or(options.current_sandbox.as_deref());
    axis_label(&options.sandboxes, value, agent)
}

impl KumuApp {
    /// Switch the session's sandbox policy: persist it, then apply it over the
    /// session's sandbox setting. A session with no live connection just stores
    /// the pick, which is re-applied when one is opened.
    fn pick_sandbox(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        self.workspace.update(cx, |workspace, cx| {
            workspace.pick(session_id, Axis::Sandbox, value, cx)
        });
    }

    /// The composer's sandbox picker: a text chip showing the policy in force,
    /// which opens the policies the agent offers.
    ///
    /// Renders nothing for an agent without a sandbox axis — and nothing before
    /// a probe or session has reported one.
    pub(super) fn sandbox_picker(
        &self,
        session: &Session,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let options = self.workspace.read(cx).session_options(session).cloned()?;
        if options.sandboxes.is_empty() {
            return None;
        }
        let label = sandbox_label(
            Some(&options),
            session.sandbox.as_deref(),
            &self.agent_name(&session.agent),
        )?;
        // The stored pick is what the user last chose; the catalogue's value is
        // what the agent says it is actually running under.
        let current = session
            .sandbox
            .clone()
            .or_else(|| options.current_sandbox.clone());

        let weak = cx.entity().downgrade();
        let chrome = PickerChrome::new(cx.theme());
        let open = self.open_picker == Some(PickerAxis::Sandbox);

        // Plain text, like the other chips: the policy name is the whole
        // affordance.
        let chip = picker_chip("sandbox-picker-chip", &chrome, label, None, open);

        Some(
            Popover::new("sandbox-picker")
                .appearance(false)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let weak = weak.clone();
                    move |open, _window, cx| {
                        let open = *open;
                        let _ = weak.update(cx, |app, cx| {
                            app.set_picker_open(PickerAxis::Sandbox, open);
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = picker_list();
                    for sandbox in &options.sandboxes {
                        let value = sandbox.value.clone();
                        let selected = current.as_deref() == Some(sandbox.value.as_str());
                        let weak = weak.clone();
                        // Name only, like the other pickers: what each policy
                        // means belongs in the docs, not in a menu row.
                        list = list.child(select_row(
                            &chrome,
                            &sandbox.name,
                            None,
                            selected,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.pick_sandbox(value.clone(), cx);
                                    app.open_picker = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    picker_menu("sandbox-picker-list", &chrome, list)
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

    use super::sandbox_label;
    use crate::agent::SandboxInfo;
    use crate::app::discovery::SessionOptions;

    fn sandbox(value: &str, name: &str) -> SandboxInfo {
        SandboxInfo {
            value: value.into(),
            name: name.into(),
        }
    }

    fn options(current_sandbox: Option<&str>) -> SessionOptions {
        SessionOptions {
            sandboxes: vec![
                sandbox("read-only", "只读"),
                sandbox("workspace-write", "工作区写入"),
            ],
            current_sandbox: current_sandbox.map(str::to_owned),
            ..Default::default()
        }
    }

    /// The stored pick wins over the agent's active value, and either resolves
    /// through the catalogue to the agent's own label.
    #[test]
    fn sandbox_label_prefers_the_stored_pick() {
        let options = options(Some("read-only"));
        assert_eq!(
            sandbox_label(Some(&options), Some("workspace-write"), "Claude Code").as_deref(),
            Some("工作区写入")
        );
        assert_eq!(
            sandbox_label(Some(&options), None, "Claude Code").as_deref(),
            Some("只读")
        );
    }

    /// A value the catalogue no longer lists still shows as itself rather than
    /// leaving the chip blank.
    #[test]
    fn sandbox_label_falls_back_to_the_raw_value() {
        let options = options(Some("read-only"));
        assert_eq!(
            sandbox_label(Some(&options), Some("gone"), "Claude Code").as_deref(),
            Some("gone")
        );
    }

    /// Unlike the mode chip, an unset pick still has something to say: kumu is
    /// not overriding anything, and the chip says so.
    #[test]
    fn sandbox_label_says_so_when_nothing_is_overridden() {
        assert_eq!(
            sandbox_label(Some(&options(None)), None, "Claude Code").as_deref(),
            Some(inherited_label("Claude Code").as_str())
        );
    }

    /// No sandbox axis means no chip, which is what keeps it off an ACP agent's
    /// toolbar.
    #[test]
    fn sandbox_label_is_none_without_a_sandbox_axis() {
        assert_eq!(sandbox_label(None, Some("read-only"), "Claude Code"), None);
        assert_eq!(
            sandbox_label(Some(&SessionOptions::default()), None, "Claude Code"),
            None
        );
    }
}
