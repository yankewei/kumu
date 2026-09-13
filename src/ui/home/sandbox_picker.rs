//! Composer sandbox picker: the session's sandbox policy, as an agent-reported
//! select.
//!
//! Separate from the mode picker on purpose: modes and the sandbox are different
//! axes, and an agent may have either, both, or neither.

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

/// The label for the sandbox chip: the user's stored pick, else the policy the
/// agent reports it is running under, resolved to a name through the catalogue.
///
/// A value the catalogue no longer lists still shows as itself rather than
/// leaving the chip blank, and an agent with no sandbox axis has no chip at all.
fn sandbox_label(
    options: Option<&SessionOptions>,
    stored: Option<&str>,
    agent: &str,
) -> Option<String> {
    let options = options?;
    if options.sandboxes.is_empty() {
        return None;
    }
    Some(match stored.or(options.current_sandbox.as_deref()) {
        Some(value) => options
            .sandboxes
            .iter()
            .find(|sandbox| sandbox.value == value)
            .map(|sandbox| sandbox.name.clone())
            .unwrap_or_else(|| value.to_string()),
        None => inherited_label(agent),
    })
}

impl KumuApp {
    /// Switch the session's sandbox policy: persist it, then apply it over the
    /// session's sandbox setting. A session with no live connection just stores
    /// the pick, which is re-applied when one is opened.
    fn pick_sandbox(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else {
            return;
        };
        self.store
            .update_session_sandbox(session_id, Some(&value))
            .ok();
        self.refresh();
        if let Some(conn) = self.conns.get(&session_id) {
            conn.send(backend::Cmd::SetSandbox(value));
        }
        cx.notify();
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
        let options = cx
            .try_global::<AgentSessionOptions>()
            .and_then(|catalogs| catalogs.0.get(session.agent.as_str()))
            .cloned()?;
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
        let open = self.sandbox_picker_open;
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
                .id("sandbox-picker-chip")
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
            Popover::new("sandbox-picker")
                .appearance(false)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let weak = weak.clone();
                    move |open, _window, cx| {
                        let open = *open;
                        let _ = weak.update(cx, |app, cx| {
                            app.sandbox_picker_open = open;
                            cx.notify();
                        });
                    }
                })
                .trigger(chip)
                .content(move |_state, _window, _cx| {
                    let mut list = v_flex().gap_0p5().p_1p5();
                    for sandbox in &options.sandboxes {
                        let value = sandbox.value.clone();
                        let selected = current.as_deref() == Some(sandbox.value.as_str());
                        let weak = weak.clone();
                        // Name only, like the other pickers: what each policy
                        // means belongs in the docs, not in a menu row.
                        list = list.child(select_row(
                            &sandbox.name,
                            None,
                            selected,
                            radius,
                            muted,
                            muted_fg,
                            accent,
                            accent_fg,
                            move |_, _, cx| {
                                let _ = weak.update(cx, |app, cx| {
                                    app.pick_sandbox(value.clone(), cx);
                                    app.sandbox_picker_open = false;
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
                                .id("sandbox-picker-list")
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

    use super::sandbox_label;
    use crate::backend::SandboxInfo;
    use crate::ui::SessionOptions;

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
