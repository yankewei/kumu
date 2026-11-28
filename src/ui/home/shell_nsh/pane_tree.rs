//! Pane chrome, `⌘F` find bar, and recursive split-tree rendering for [`super::NshHomeShell`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures::channel::mpsc::UnboundedSender;
use nsh_base::{h_resizable, resizable_panel, theme as active_theme, v_resizable};
use nsh_core::element::native;
use nsh_core::prelude::*;

use super::{HomeShellAction, LayoutNodeSnapshot, TERMINAL_COMPONENT, send};
use crate::agent::AgentState;
use crate::app::layout::{PaneId, SplitAxis};
use crate::nsh_ui::agent_picker::AgentPickerAnchor;
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::{
    ControlTone, PillTone, kumu_icon_button, kumu_input, kumu_list_button, pill,
};
use crate::nsh_ui::theme::{link_color, success_color, text_xs_size, warning_color};
use crate::ui::home::terminal::PaneChromeSnapshot;

pub(super) type PaneSignalMap = Rc<RefCell<HashMap<PaneId, Signal<Option<PaneChromeSnapshot>>>>>;

fn render_agent_badge(state: AgentState, unseen: bool) -> AnyElement {
    let color = move || match state {
        AgentState::Running => link_color(),
        AgentState::Waiting => warning_color(),
        AgentState::Completed => success_color(),
        AgentState::Failed => active_theme().destructive,
    };
    let tint = if state == AgentState::Running {
        0.14
    } else {
        0.16
    };
    let glyph: AnyElement = match state {
        AgentState::Running => icon_svg("icons/loader-circle.svg", 12.0)
            .app_class("animate-spin")
            .text_color(color)
            .into_element(),
        AgentState::Waiting => view()
            .w(px(7.0))
            .h(px(7.0))
            .rounded(px(3.5))
            .bg(color)
            .into_element(),
        AgentState::Completed => icon_svg("icons/check.svg", 12.0)
            .text_color(color)
            .into_element(),
        AgentState::Failed => icon_svg("icons/triangle-alert.svg", 12.0)
            .text_color(color)
            .into_element(),
    };
    let mut badge = view()
        .flex_row()
        .flex_shrink_0()
        .h(px(20.0))
        .gap_1()
        .px_1p5()
        .items_center()
        .rounded(move || active_theme().radius)
        .bg(move || color().opacity(tint))
        .child(glyph)
        .child(
            text(state.label())
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(color),
        );
    if unseen {
        badge = badge.border_1().border_color(color);
    }
    badge.into_element()
}

pub(super) fn header_icon_button(
    icon: &'static str,
    tooltip_label: &'static str,
    on_press: impl Fn() + 'static,
) -> View {
    kumu_icon_button(
        tooltip_label,
        icon_svg(icon, 14.0).text_color(move || active_theme().muted_foreground),
        ControlTone::Default,
        on_press,
    )
}

fn pane_signal(signals: &PaneSignalMap, pane_id: PaneId) -> Signal<Option<PaneChromeSnapshot>> {
    *signals
        .borrow_mut()
        .entry(pane_id)
        .or_insert_with(|| signal(None))
}

fn render_pane_leaf(
    pane_id: PaneId,
    pane_sig: Signal<Option<PaneChromeSnapshot>>,
    tx: Option<UnboundedSender<HomeShellAction>>,
) -> AnyElement {
    let tx_hdr = tx.clone();
    let tx_find = tx.clone();

    let header = dynamic(move || {
        let Some(snap) = pane_sig.get() else {
            return view().h(px(34.0)).w_full().into_element();
        };
        let active = snap.active;
        let can_close = snap.can_close;
        let can_zoom = snap.can_zoom;
        let zoomed = snap.zoomed;
        let tx_focus = tx_hdr.clone();
        let tx_agent = tx_hdr.clone();
        let tx_sr = tx_hdr.clone();
        let tx_sd = tx_hdr.clone();
        let tx_zoom = tx_hdr.clone();
        let tx_close = tx_hdr.clone();
        let focus_label = format!("聚焦窗格 {}", snap.name);

        let mut row = view()
            .flex_row()
            .flex_shrink_0()
            .w_full()
            .h(px(34.0))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_b_color(move || active_theme().border)
            .bg(move || {
                if active {
                    active_theme().secondary.opacity(0.45)
                } else {
                    AdaptiveColor::TRANSPARENT
                }
            });
        let focus_button = kumu_list_button(focus_label, active, ControlTone::Pane)
            .flex_1()
            .min_w_0()
            .gap_2()
            .on_press(move || send(&tx_focus, HomeShellAction::FocusPane(pane_id)))
            .child(
                icon_svg("icons/square-terminal.svg", 14.0)
                    .text_color(move || active_theme().muted_foreground),
            )
            .child(
                text(snap.name)
                    .flex_shrink_0()
                    .max_w(px(180.0))
                    .min_w_0()
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().foreground),
            )
            .child(
                text(snap.cwd)
                    .flex_1()
                    .min_w_0()
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().muted_foreground),
            );
        row = row.child(focus_button);

        // Kept in one always-present group so the header actions keep
        // their place as these come and go.
        let mut extras = view().flex_row().items_center().gap_2().flex_shrink_0();
        if let Some(note) = snap.note {
            extras = extras.child(
                text(note)
                    .flex_shrink_0()
                    .text_size(text_xs_size)
                    .text_color(warning_color),
            );
        }
        if let Some(status) = snap.agent_status {
            extras = extras.child(render_agent_badge(status, snap.agent_unseen));
        }
        if zoomed {
            extras = extras.child(pill("已最大化", PillTone::Neutral).flex_shrink_0());
        }
        row = row.child(extras);

        let agent_anchor = signal(AgentPickerAnchor {
            x: window().width.0 - 16.0,
            y: 54.0,
        });
        let anchor_on_press = agent_anchor;
        let agent_trigger = kumu_icon_button(
            "新建 Agent 窗格",
            icon_svg("icons/bot.svg", 14.0).text_color(move || active_theme().muted_foreground),
            ControlTone::Default,
            move || {
                send(
                    &tx_agent,
                    HomeShellAction::OpenAgentPicker {
                        pane: pane_id,
                        anchor: agent_anchor.get_untracked(),
                    },
                );
            },
        )
        .on_press_down(move |ev: &PressEvent| {
            anchor_on_press.set(AgentPickerAnchor {
                x: ev.page_position.x.0,
                y: ev.page_position.y.0,
            });
        });

        row = row
            .child(agent_trigger)
            .child(header_icon_button(
                "split-right.svg",
                "向右分屏 ⌘D",
                move || send(&tx_sr, HomeShellAction::SplitRight(pane_id)),
            ))
            .child(header_icon_button(
                "split-down.svg",
                "向下分屏 ⌘⇧D",
                move || send(&tx_sd, HomeShellAction::SplitDown(pane_id)),
            ));

        if can_zoom || zoomed {
            let (icon, tooltip_label) = if zoomed {
                ("icons/minimize-2.svg", "还原分屏 ⌘⇧↩")
            } else {
                ("icons/maximize-2.svg", "最大化窗格 ⌘⇧↩")
            };
            row = row.child(header_icon_button(icon, tooltip_label, move || {
                send(&tx_zoom, HomeShellAction::ToggleZoomPane(pane_id));
            }));
        }

        if can_close {
            row = row.child(header_icon_button(
                "icons/close.svg",
                "关闭窗格 ⌘⇧W",
                move || send(&tx_close, HomeShellAction::ClosePane(pane_id)),
            ));
        }

        row.into_element()
    });

    let find_open = move || pane_sig.with(|p| p.as_ref().is_some_and(|c| c.find.is_some()));
    let find_bar = when(find_open, move || {
        let initial_find = pane_sig.get_untracked().and_then(|c| c.find);
        let initial_needle = initial_find
            .as_ref()
            .map(|f| f.needle.clone())
            .unwrap_or_default();
        let initial_nonce = initial_find.as_ref().map_or(0, |f| f.focus_nonce);

        let draft = signal(initial_needle);
        let input_ref = element_ref();
        let last_nonce = stored(initial_nonce);

        // Re-focus (and select-all) whenever ⌘F is pressed again while open.
        effect(move || {
            let nonce = pane_sig.with(|p| {
                p.as_ref()
                    .and_then(|c| c.find.as_ref())
                    .map_or(0, |f| f.focus_nonce)
            });
            let changed = last_nonce.with(|prev| {
                if nonce > *prev {
                    *prev = nonce;
                    true
                } else {
                    false
                }
            });
            if changed {
                input_ref.focus();
            }
        });

        let count_label = move || {
            pane_sig.with(|p| {
                p.as_ref()
                    .and_then(|c| c.find.as_ref())
                    .map(|f| f.count_label.clone())
                    .unwrap_or_default()
            })
        };
        let has_matches = move || {
            pane_sig.with(|p| {
                p.as_ref()
                    .and_then(|c| c.find.as_ref())
                    .is_some_and(|f| f.has_matches)
            })
        };

        let tx_change = tx_find.clone();
        let tx_key = tx_find.clone();
        let tx_older = tx_find.clone();
        let tx_newer = tx_find.clone();
        let tx_close = tx_find.clone();

        view()
            .absolute()
            .top_2()
            .right_3()
            .w(px(340.0))
            .app_class("max-w-[92%]")
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(move || active_theme().radius)
            .border_1()
            .border_color(move || active_theme().border)
            .bg(move || active_theme().secondary)
            .shadow_md()
            .on_key_down(move |ev: &nsh_core::KeyEvent| {
                if ev.key == "Escape" {
                    send(&tx_key, HomeShellAction::FindClose(pane_id));
                } else if ev.key == "Enter" {
                    send(
                        &tx_key,
                        HomeShellAction::FindStep {
                            pane: pane_id,
                            older: !ev.modifiers.shift,
                        },
                    );
                }
            })
            .child(
                kumu_input("在当前终端中查找")
                    .element_ref(&input_ref)
                    .auto_focus(true)
                    .select_text_on_focus(true)
                    .flex_1()
                    .min_w_0()
                    .py_0p5()
                    .px_1p5()
                    .value(move || draft.get())
                    .on_change(move |v: &str| {
                        draft.set(v.to_owned());
                        send(
                            &tx_change,
                            HomeShellAction::FindChange {
                                pane: pane_id,
                                needle: v.to_owned(),
                            },
                        );
                    }),
            )
            .child(
                text(count_label)
                    .style_when(move || count_label().is_empty(), |s| s.hidden())
                    .flex_shrink_0()
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().muted_foreground),
            )
            .child(
                header_icon_button(
                    "icons/chevron-up.svg",
                    "查找更早的内容（Enter）",
                    move || {
                        send(
                            &tx_older,
                            HomeShellAction::FindStep {
                                pane: pane_id,
                                older: true,
                            },
                        );
                        input_ref.focus();
                    },
                )
                .opacity(move || if has_matches() { 1.0_f32 } else { 0.4_f32 }),
            )
            .child(
                header_icon_button(
                    "icons/chevron-down.svg",
                    "查找更新的内容（⇧Enter）",
                    move || {
                        send(
                            &tx_newer,
                            HomeShellAction::FindStep {
                                pane: pane_id,
                                older: false,
                            },
                        );
                        input_ref.focus();
                    },
                )
                .opacity(move || if has_matches() { 1.0_f32 } else { 0.4_f32 }),
            )
            .child(header_icon_button(
                "icons/x.svg",
                "关闭查找",
                move || {
                    send(&tx_close, HomeShellAction::FindClose(pane_id));
                },
            ))
    });

    let is_active = move || pane_sig.with(|p| p.as_ref().is_some_and(|c| c.active));

    view()
        .flex_col()
        .size_full()
        .min_h_0()
        .min_w_0()
        .border_1()
        .border_color(move || {
            if is_active() {
                link_color()
            } else {
                AdaptiveColor::TRANSPARENT
            }
        })
        .bg(move || active_theme().background)
        .child(header)
        .child(
            view()
                .relative()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .child(
                    native(TERMINAL_COMPONENT)
                        .size_full()
                        .prop("pane", serde_json::json!(pane_id.0)),
                )
                .child(find_bar),
        )
        .into_element()
}

pub(super) fn render_pane_tree(
    node: &LayoutNodeSnapshot,
    pane_signals: &PaneSignalMap,
    tx: Option<UnboundedSender<HomeShellAction>>,
) -> AnyElement {
    match node {
        LayoutNodeSnapshot::Leaf(pane_id) => {
            render_pane_leaf(*pane_id, pane_signal(pane_signals, *pane_id), tx)
        }
        LayoutNodeSnapshot::Group {
            id,
            axis,
            ratios,
            children,
        } => {
            let group_id = *id;
            let tx_resize = tx.clone();
            let mut group = match axis {
                SplitAxis::Horizontal => h_resizable()
                    .handle_class("w-px bg-border group-hover:bg-primary group-active:bg-primary"),
                SplitAxis::Vertical => v_resizable()
                    .handle_class("h-px bg-border group-hover:bg-primary group-active:bg-primary"),
            };
            group = group.on_resize(move |sizes: &Vec<Points>| {
                let total: f32 = sizes.iter().map(|s| s.0).sum();
                if total > 0.0 && total.is_finite() {
                    let next: Vec<f32> = sizes.iter().map(|s| s.0 / total).collect();
                    if next.iter().all(|r| r.is_finite() && *r > 0.0) {
                        send(
                            &tx_resize,
                            HomeShellAction::ResizeGroup {
                                group_id,
                                ratios: next,
                            },
                        );
                    }
                }
            });

            let count = children.len().max(1);
            for (index, child) in children.iter().enumerate() {
                let child_el = render_pane_tree(child, pane_signals, tx.clone());
                let mut panel = resizable_panel().min_size(px(120.0));
                if ratios.len() == count {
                    panel = panel.size(Some(px(ratios[index] * 1000.0)));
                }
                // Reserve 1px after leading divider so a focused pane's 1px
                // border stays visible on all four sides.
                let wrapped = if index > 0 {
                    match axis {
                        SplitAxis::Horizontal => view()
                            .size_full()
                            .pl(px(1.0))
                            .child(child_el)
                            .into_element(),
                        SplitAxis::Vertical => view()
                            .size_full()
                            .pt(px(1.0))
                            .child(child_el)
                            .into_element(),
                    }
                } else {
                    child_el
                };
                group = group.child(panel.child(wrapped));
            }
            group.into_element()
        }
    }
}
