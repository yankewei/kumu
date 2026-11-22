//! Anchored Pure Rust NSH picker for starting a new agent in a pane.
//!
//! A GPUI-level overlay (like `dialogs` / `command_palette`) rather than an
//! NSH `menu`: menu popups are painted above the pane's terminal but its
//! native component still takes the pointer, so the rows were not clickable.
//! This overlay sits above the whole home shell and receives every press.

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::mount::MountRegistry;
use super::theme::{appearance, nsh_theme};
use crate::theme::KumuTheme;

/// The press that asked for the picker, in the home shell's coordinates
/// (the NSH view is the window content below the GPUI title bar).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentPickerAnchor {
    pub x: f32,
    pub y: f32,
}

/// One selectable agent; computed on the host thread so the engine never
/// touches the filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPickerItem {
    pub id: String,
    pub name: String,
    pub logo: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentPickerOutcome {
    Selected(String),
    Dismissed,
}

struct AgentPickerMount {
    anchor: AgentPickerAnchor,
    items: Vec<AgentPickerItem>,
    outcome_tx: UnboundedSender<AgentPickerOutcome>,
}

static MOUNTS: MountRegistry<AgentPickerMount> = MountRegistry::new();

mod view {
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::theme as active_theme;
    use nsh_core::prelude::*;

    use super::{AgentPickerAnchor, AgentPickerItem, AgentPickerOutcome, MOUNTS};
    use crate::nsh_ui::assets::{asset_src, icon_svg};
    use crate::nsh_ui::components::{ControlTone, kumu_list_button};
    use crate::nsh_ui::theme::text_xs_size;
    use crate::ui::titlebar::TITLE_BAR_HEIGHT;

    const CARD_WIDTH: f32 = 208.0;
    const CARD_MARGIN: f32 = 8.0;

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct AgentPickerRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<AgentPickerOutcome>>, outcome: AgentPickerOutcome) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(outcome);
        }
    }

    /// Each agent's accent, used for its glyph and the tile behind it so the
    /// monochrome brand marks still read as distinct, colored choices.
    fn agent_accent(id: &str) -> AdaptiveColor {
        match id {
            "opencode" => AdaptiveColor::rgb(0x475569, 0x94a3b8),
            "omp" => AdaptiveColor::rgb(0xd97706, 0xfbbf24),
            "codex" => AdaptiveColor::rgb(0x0d8a6a, 0x34d399),
            "claude" => AdaptiveColor::rgb(0xc26144, 0xf0936e),
            "pi" => AdaptiveColor::rgb(0x7c3aed, 0xa78bfa),
            "cursor" => AdaptiveColor::rgb(0x4f46e5, 0x818cf8),
            "grok" => AdaptiveColor::rgb(0x0f172a, 0xcbd5e1),
            "agy" => AdaptiveColor::rgb(0x6366f1, 0x93c5fd),
            _ => AdaptiveColor::rgb(0x64748b, 0x94a3b8),
        }
    }

    fn agent_logo(item: &AgentPickerItem) -> AnyElement {
        let accent = agent_accent(&item.id);
        let glyph: AnyElement = match item.logo {
            Some(path) if path.ends_with(".svg") => icon_svg(path, 14.0)
                .text_color(move || accent)
                .into_element(),
            Some(path) => image(asset_src(path))
                .w(px(14.0))
                .h(px(14.0))
                .flex_shrink_0()
                .into_element(),
            None => icon_svg("icons/bot.svg", 14.0)
                .text_color(move || accent)
                .into_element(),
        };
        view()
            .flex_shrink_0()
            .w(px(20.0))
            .h(px(20.0))
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(move || accent.opacity(0.16))
            .border_1()
            .border_color(move || accent.opacity(0.35))
            .child(glyph)
            .into_element()
    }

    fn render_row(
        item: &AgentPickerItem,
        tx: &Option<UnboundedSender<AgentPickerOutcome>>,
    ) -> AnyElement {
        let id = item.id.clone();
        let tx_row = tx.clone();
        let label = format!("新建 {} 窗格", item.name);
        kumu_list_button(label, false, ControlTone::Default)
            .flex_shrink_0()
            .h(px(30.0))
            .px_2()
            .gap_2()
            .on_press(move || send(&tx_row, AgentPickerOutcome::Selected(id.clone())))
            .child(agent_logo(item))
            .child(
                text(item.name.clone())
                    .flex_1()
                    .min_w_0()
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().foreground),
            )
            .into_element()
    }

    #[component]
    pub(super) fn AgentPickerRoot(props: AgentPickerRootProps) -> impl IntoElement {
        let mount = MOUNTS.take(props.mount_id.get_untracked());
        let (anchor, items, tx) = match mount {
            Some(m) => (m.anchor, m.items, Some(m.outcome_tx)),
            None => (AgentPickerAnchor::default(), Vec::new(), None),
        };

        let tx_backdrop = tx.clone();
        let tx_escape = tx.clone();
        let on_key = move |ev: &nsh_core::KeyEvent| {
            if ev.key == "Escape" {
                send(&tx_escape, AgentPickerOutcome::Dismissed);
            }
        };

        // The card hangs to the left of the press, so it stays on screen even
        // at the right edge of a pane. The shell's root starts below the GPUI
        // title bar; the overlay spans the whole window, so add that bar back.
        let left = (anchor.x - CARD_WIDTH + 20.0).max(CARD_MARGIN);
        let top = anchor.y + TITLE_BAR_HEIGHT + 12.0;

        let rows: Vec<AnyElement> = if items.is_empty() {
            vec![
                view()
                    .flex_row()
                    .w_full()
                    .h(px(30.0))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(move || active_theme().radius)
                    .child(
                        icon_svg("icons/bot.svg", 14.0)
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .child(
                        text("未安装 Agent")
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .into_element(),
            ]
        } else {
            items.iter().map(|item| render_row(item, &tx)).collect()
        };

        let card = view()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(CARD_WIDTH))
            .p_1()
            .flex_col()
            .gap_0p5()
            .rounded(px(8.0))
            .bg(move || active_theme().popover)
            .border_1()
            .border_color(move || active_theme().border)
            .shadow_lg()
            .overflow_hidden()
            .children(rows);

        view()
            .relative()
            .size_full()
            .focusable(true)
            .on_key_down(on_key)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(
                view()
                    .absolute()
                    .inset(px(0.0))
                    .on_press(move || send(&tx_backdrop, AgentPickerOutcome::Dismissed)),
            )
            .child(card)
    }
}

/// Active new-agent picker overlay owned by `KumuApp`.
pub struct ActiveAgentPicker {
    mount_id: u64,
    pub app_view: Entity<AppView>,
    _task: Task<()>,
}

impl Drop for ActiveAgentPicker {
    fn drop(&mut self) {
        let _ = MOUNTS.take(self.mount_id);
    }
}

impl ActiveAgentPicker {
    pub fn open<V: 'static>(
        anchor: AgentPickerAnchor,
        items: Vec<AgentPickerItem>,
        theme: &KumuTheme,
        window: &mut Window,
        cx: &mut Context<V>,
        on_outcome: impl FnOnce(&mut V, AgentPickerOutcome, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (outcome_tx, mut outcome_rx) = unbounded::<AgentPickerOutcome>();
        let mount_id = MOUNTS.register(AgentPickerMount {
            anchor,
            items,
            outcome_tx,
        });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Agent Picker".to_owned(),
                transparent: true,
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<
                view::AgentPickerRoot,
            >(move || {
                view::AgentPickerRootProps {
                    mount_id: mount_id.into(),
                }
            })));
            view
        });

        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = outcome_rx
                .next()
                .await
                .unwrap_or(AgentPickerOutcome::Dismissed);
            let _ = this.update_in(cx, |host, window, cx| {
                on_outcome(host, outcome, window, cx);
            });
        });

        Self {
            mount_id,
            app_view,
            _task: task,
        }
    }

    pub fn sync_theme(&self, theme: &KumuTheme, cx: &mut gpui::App) {
        let (mapped, mode) = (nsh_theme(theme), appearance(theme.mode));
        self.app_view.update(cx, |view, cx| {
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn picking_an_agent_reports_it() {
        let (tx, mut rx) = unbounded::<AgentPickerOutcome>();
        let mount_id = MOUNTS.register(AgentPickerMount {
            anchor: AgentPickerAnchor::default(),
            items: vec![AgentPickerItem {
                id: "codex".into(),
                name: "Codex".into(),
                logo: Some("codex.svg"),
            }],
            outcome_tx: tx,
        });
        let root = nsh_core::element::Root::new::<view::AgentPickerRoot>(move || {
            view::AgentPickerRootProps {
                mount_id: mount_id.into(),
            }
        });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start picker NSH");
        app.wait_for("Codex").expect("agent renders");
        app.press("Codex").expect("press agent");
        assert_eq!(
            rx.try_recv().ok(),
            Some(AgentPickerOutcome::Selected("codex".into()))
        );
    }
}
