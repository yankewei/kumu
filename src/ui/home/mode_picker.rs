//! Composer mode picker: the session's mode, as an agent-reported select.

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

/// The label for the mode chip: the user's stored pick while the agent still
/// offers it, else the value the agent reports as active, resolved to a name
/// through the catalogue.
///
/// The membership test is what keeps one stored value from leaking across axes:
/// an agent with both a work mode and an access policy (Claude Code) stores
/// whichever the user picked last, so a stored `acceptEdits` must drop out of
/// this chip and let the reported work mode take over — the same rule the
/// reasoning picker applies to a pick its agent no longer lists. A *reported*
/// value the catalogue does not list still shows as itself rather than leaving
/// the chip blank. No catalogue — or a catalogue with no modes — means there is
/// no such axis and therefore no chip at all.
fn mode_label(
    options: Option<&SessionOptions>,
    stored: Option<&str>,
    agent: &str,
) -> Option<String> {
    let options = options?;
    if options.modes.is_empty() {
        return None;
    }
    let value = stored
        .filter(|value| options.modes.iter().any(|mode| mode.value == *value))
        .or(options.current_mode.as_deref());
    Some(match value {
        Some(value) => options
            .modes
            .iter()
            .find(|mode| mode.value == value)
            .map(|mode| mode.name.clone())
            .unwrap_or_else(|| value.to_string()),
        None => inherited_label(agent),
    })
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
        self.store
            .update_session_mode(session_id, Some(&value))
            .ok();
        self.refresh();
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(backend::Cmd::SetMode(value));
        }
        cx.notify();
    }

    /// The composer's mode picker: a text chip showing the mode in force, which
    /// opens the modes the agent offers.
    ///
    /// Renders nothing for an agent that reports no modes at all — the axis does
    /// not exist there — and the inherited label when it reports modes but
    /// nothing in force yet.
    pub(super) fn mode_picker(&self, session: &Session, cx: &Context<Self>) -> Option<AnyElement> {
        let options = cx
            .try_global::<AgentSessionOptions>()
            .and_then(|catalogs| catalogs.0.get(session.agent.as_str()))
            .cloned()?;
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
        let open = self.mode_picker_open;
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

        // Plain text, like the model chip: the mode name is the whole
        // affordance, and the agent's own labels are short.
        let chip = ChipTrigger {
            element: h_flex()
                .id("mode-picker-chip")
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
            Popover::new("mode-picker")
                .appearance(false)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let weak = weak.clone();
                    move |open, _window, cx| {
                        let open = *open;
                        let _ = weak.update(cx, |app, cx| {
                            app.mode_picker_open = open;
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = v_flex().gap_0p5().p_1p5();
                    for mode in &options.modes {
                        let value = mode.value.clone();
                        let selected = current.as_deref() == Some(mode.value.as_str());
                        let weak = weak.clone();
                        // Name only: the agent's description of a mode is
                        // longer than the choice is worth.
                        list = list.child(select_row(
                            &mode.name,
                            None,
                            selected,
                            radius,
                            muted,
                            muted_fg,
                            accent,
                            accent_fg,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.pick_mode(value.clone(), cx);
                                    app.mode_picker_open = false;
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    // Sized to the names, like Waku's menus: the card is
                    // content-driven with a cap, not a fixed width, so a list
                    // of bare labels does not reserve room for a description it
                    // has none of (Waku caps its menu cards at 320).
                    v_flex()
                        .max_w(px(320.))
                        .max_h(px(320.))
                        .rounded(radius)
                        .border_1()
                        .border_color(border)
                        .bg(background)
                        .shadow_lg()
                        .overflow_hidden()
                        .child(div().id("mode-picker-list").overflow_y_scroll().child(list))
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
    use crate::backend::ModeInfo;
    use crate::ui::SessionOptions;

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
