//! Composer mode picker: the session's mode, as an agent-reported select.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use gpui_kit::component::popover::Popover;

use super::{KumuApp, PickerAxis};
use crate::app::Axis;
use crate::app::discovery::SessionOptions;
use crate::db::Session;
use crate::ui::widgets::{
    PickerChrome, axis_label, in_force, picker_chip, picker_list, picker_menu, select_row,
};

/// The label for the mode chip: the value in force on the work-mode axis,
/// resolved to the agent's own name for it, and no chip at all when the agent
/// reports no modes.
///
/// On Claude Code this axis shares its stored value with the access policy — one
/// permission mode, two chips — so a stored `acceptEdits` belongs to the other
/// chip and drops out here, letting the reported work mode take over.
fn mode_label(
    options: Option<&SessionOptions>,
    stored: Option<&str>,
    agent: &str,
) -> Option<String> {
    let options = options?;
    let value = in_force(&options.modes, stored, options.current_mode.as_deref());
    axis_label(&options.modes, value, agent)
}

impl KumuApp {
    /// Switch the session's mode: persist it, then apply it over the session's
    /// mode config option. A session with no live connection just stores the
    /// pick, which is re-applied when one is opened.
    ///
    /// The access chip calls this too: an agent that has both axes (Claude Code)
    /// stores and reports them as one permission mode, so a pick on either chip
    /// is the same write.
    pub(super) fn pick_mode(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        self.workspace.update(cx, |workspace, cx| {
            workspace.pick(session_id, Axis::Mode, value, cx)
        });
    }

    /// The composer's mode picker: a text chip showing the mode in force, which
    /// opens the modes the agent offers.
    ///
    /// Renders nothing for an agent that reports no modes at all — the axis does
    /// not exist there — and the inherited label when it reports modes but
    /// nothing in force yet.
    pub(super) fn mode_picker(&self, session: &Session, cx: &Context<Self>) -> Option<AnyElement> {
        let options = self.workspace.read(cx).session_options(session).cloned()?;
        let label = mode_label(
            Some(&options),
            session.mode.as_deref(),
            &self.agent_name(&session.agent),
        )?;
        // The stored pick is what the user last chose; the catalog's value is
        // what the agent says it is actually in.
        let current = session
            .mode
            .clone()
            .or_else(|| options.current_mode.clone());

        let weak = cx.entity().downgrade();
        let chrome = PickerChrome::new(cx.theme());
        let open = self.open_picker == Some(PickerAxis::Mode);

        // Plain text, like the model chip: the mode name is the whole
        // affordance, and the agent's own labels are short.
        let chip = picker_chip("mode-picker-chip", &chrome, label, None, open);

        Some(
            Popover::new("mode-picker")
                .appearance(false)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let weak = weak.clone();
                    move |open, _window, cx| {
                        let open = *open;
                        let _ = weak.update(cx, |app, cx| {
                            app.set_picker_open(PickerAxis::Mode, open);
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = picker_list();
                    for mode in &options.modes {
                        let value = mode.value.clone();
                        let selected = current.as_deref() == Some(mode.value.as_str());
                        let weak = weak.clone();
                        // Name only: the agent's description of a mode is
                        // longer than the choice is worth.
                        list = list.child(select_row(
                            &chrome,
                            &mode.name,
                            None,
                            selected,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.pick_mode(value.clone(), cx);
                                    app.open_picker = None;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    picker_menu("mode-picker-list", &chrome, list)
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

    use super::mode_label;
    use crate::agent::ModeInfo;
    use crate::app::discovery::SessionOptions;

    fn mode(value: &str, name: &str) -> ModeInfo {
        ModeInfo {
            value: value.into(),
            name: name.into(),
        }
    }

    fn options(current_mode: Option<&str>) -> SessionOptions {
        SessionOptions {
            modes: vec![mode("ask", "Ask"), mode("build", "Build")],
            current_mode: current_mode.map(str::to_owned),
            ..Default::default()
        }
    }

    /// The stored pick wins over the agent's active value, and either resolves
    /// through the catalog to the agent's own label.
    #[test]
    fn mode_label_prefers_the_stored_pick() {
        let options = options(Some("ask"));
        assert_eq!(
            mode_label(Some(&options), Some("build"), "Claude Code").as_deref(),
            Some("Build")
        );
        assert_eq!(
            mode_label(Some(&options), None, "Claude Code").as_deref(),
            Some("Ask")
        );
    }

    /// A pick the agent no longer lists yields to what it reports, and a
    /// *reported* value the catalogue does not list still shows as itself
    /// rather than leaving the chip blank.
    #[test]
    fn mode_label_yields_a_stale_pick_but_shows_a_stale_report() {
        let listed = options(Some("ask"));
        assert_eq!(
            mode_label(Some(&listed), Some("gone"), "Claude Code").as_deref(),
            Some("Ask"),
            "the report decides once the pick is off the menu"
        );

        let unlisted = SessionOptions {
            current_mode: Some("gone".into()),
            ..options(None)
        };
        assert_eq!(
            mode_label(Some(&unlisted), None, "Claude Code").as_deref(),
            Some("gone")
        );
    }

    /// No modes reported means the axis does not exist, which is what keeps the
    /// chip off the toolbar (an agent with no mode selector has nothing to open).
    #[test]
    fn mode_label_is_none_without_a_mode() {
        assert_eq!(mode_label(None, Some("ask"), "Claude Code"), None);
        assert_eq!(
            mode_label(Some(&SessionOptions::default()), None, "Claude Code"),
            None
        );
    }

    /// An agent that offers modes but reports none in force still gets a chip:
    /// Claude Code's probe claims nothing until a session exists, and a chip
    /// that hides until then is a menu the user cannot open.
    #[test]
    fn mode_label_says_so_when_nothing_is_in_force() {
        assert_eq!(
            mode_label(Some(&options(None)), None, "Claude Code").as_deref(),
            Some(inherited_label("Claude Code").as_str())
        );
    }
}
