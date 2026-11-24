//! Card and row renderers for the In-Progress, Pending Todo, and Done lanes.

use std::collections::HashMap;

use futures::channel::mpsc::UnboundedSender;
use nsh_base::theme as active_theme;
use nsh_core::ContextMenuEvent;
use nsh_core::element::AccessibilityRole;
use nsh_core::element::desktop::{popup_menu, popup_menu_at};
use nsh_core::prelude::*;

use super::menus::{CardContextMenuArgs, build_card_context_menu_items, build_dispatch_menu_items};
use super::{DragCandidate, DragState, render_source_badge, send_action};
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoStatus};
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::meta_text;
use crate::nsh_ui::theme::{link_color, success_color, text_xs_size, warning_color};
use crate::plugins::AgentPaneInfo;
use crate::plugins::todo::{TodoAction, TodoBoardSnapshot, short_agent_name};

pub(super) struct CardContext<'a, F> {
    pub tx: &'a Option<UnboundedSender<TodoAction>>,
    pub snap: &'a TodoBoardSnapshot,
    pub drag: DragState,
    pub open_detail: F,
}

pub(super) fn render_in_progress_card<F>(
    ctx: &CardContext<'_, F>,
    item: &TodoItem,
    pane_map: &HashMap<u64, AgentPaneInfo>,
) -> AnyElement
where
    F: Fn(TodoItem) + Copy + 'static,
{
    let live_pane = item.pane_id.and_then(|p| pane_map.get(&p));
    let live_state = live_pane.and_then(|p| p.state);
    let needs_review = item.awaiting_review || live_state == Some(AgentState::Completed);
    let accent_kind: Option<u8> = if live_state == Some(AgentState::Waiting) {
        Some(0)
    } else if needs_review {
        Some(1)
    } else if live_state == Some(AgentState::Failed) {
        Some(2)
    } else {
        None
    };

    let (pill_kind, pill_icon, pill_text) = match (live_pane, live_state) {
        (Some(pane), Some(AgentState::Waiting)) => (
            0u8,
            None,
            format!("{} · 等待输入", short_agent_name(&pane.agent)),
        ),
        (Some(pane), Some(AgentState::Completed)) => (
            1u8,
            Some("icons/check.svg"),
            format!("{} · 待确认", short_agent_name(&pane.agent)),
        ),
        (Some(pane), _) if needs_review => (
            1u8,
            Some("icons/check.svg"),
            format!("{} · 待确认", short_agent_name(&pane.agent)),
        ),
        (Some(pane), Some(AgentState::Failed)) => (
            2u8,
            Some("icons/triangle-alert.svg"),
            format!("{} · 失败", short_agent_name(&pane.agent)),
        ),
        (Some(pane), Some(AgentState::Running)) => (
            3u8,
            Some("icons/loader-circle.svg"),
            format!("{} · 运行中", short_agent_name(&pane.agent)),
        ),
        (Some(pane), None) => (
            4u8,
            None,
            format!("{} · 空闲", short_agent_name(&pane.agent)),
        ),
        (None, _) if needs_review => (
            1u8,
            Some("icons/check.svg"),
            item.agent_id
                .as_deref()
                .map(|a| format!("{} · 待确认", short_agent_name(a)))
                .unwrap_or_else(|| "待确认".to_string()),
        ),
        (None, _) => (
            4u8,
            None,
            item.agent_id
                .as_deref()
                .map(|a| format!("{} · 窗格已关闭", short_agent_name(a)))
                .unwrap_or_else(|| "未关联 Agent".to_string()),
        ),
    };

    let pill_fg = move || match pill_kind {
        0 => warning_color(),
        1 => success_color(),
        2 => active_theme().destructive,
        3 => link_color(),
        _ => active_theme().muted_foreground,
    };
    let pill_bg = move || match pill_kind {
        0 => warning_color().opacity(0.16),
        1 => success_color().opacity(0.16),
        2 => active_theme().destructive.opacity(0.16),
        3 => link_color().opacity(0.14),
        _ => active_theme().sidebar_accent.opacity(0.65),
    };

    let pill_el = view()
        .flex_row()
        .min_w_0()
        .h(px(20.0))
        .px_1p5()
        .gap_1()
        .rounded(px(5.0))
        .bg(pill_bg)
        .items_center()
        .child(match pill_icon {
            Some(path) => icon_svg(path, 11.0).text_color(pill_fg).into_element(),
            None => view()
                .w(px(6.0))
                .h(px(6.0))
                .rounded(px(3.0))
                .flex_shrink_0()
                .bg(pill_fg)
                .into_element(),
        })
        .child(
            text(pill_text)
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(pill_fg),
        );

    let id = item.id;
    let jump_pane = live_pane.map(|p| p.pane);
    let right_cta: Option<AnyElement> = if needs_review {
        let tx = ctx.tx.clone();
        Some(
            view()
                .role(AccessibilityRole::Button)
                .flex_shrink_0()
                .h(px(20.0))
                .px_2()
                .rounded(px(5.0))
                .bg(move || success_color().opacity(0.16))
                .hover(|s| s.bg(move || success_color().opacity(0.28)))
                .items_center()
                .justify_center()
                .on_press(move || {
                    send_action(
                        &tx,
                        TodoAction::SetStatus {
                            id,
                            status: TodoStatus::Done,
                        },
                    );
                })
                .child(
                    text("确认")
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(success_color),
                )
                .into_element(),
        )
    } else if live_state == Some(AgentState::Waiting) {
        let tx = ctx.tx.clone();
        Some(
            view()
                .role(AccessibilityRole::Button)
                .flex_shrink_0()
                .h(px(20.0))
                .px_2()
                .rounded(px(5.0))
                .bg(move || warning_color().opacity(0.16))
                .hover(|s| s.bg(move || warning_color().opacity(0.28)))
                .items_center()
                .justify_center()
                .on_press(move || {
                    if let Some(pane) = jump_pane {
                        send_action(&tx, TodoAction::RevealPane(pane));
                    }
                })
                .child(
                    text("去回复")
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(warning_color),
                )
                .into_element(),
        )
    } else if live_pane.is_none() {
        let tx = ctx.tx.clone();
        let menu_item = item.clone();
        let installed = ctx.snap.installed_agents.clone();
        let panes = ctx.snap.live_panes.clone();
        let def = ctx.snap.default_agent.clone();
        Some(
            view()
                .role(AccessibilityRole::Button)
                .flex_shrink_0()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(20.0))
                .px_2()
                .rounded(px(5.0))
                .bg(move || active_theme().sidebar)
                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                .on_press(move || {
                    let items = build_dispatch_menu_items(
                        tx.clone(),
                        menu_item.clone(),
                        installed.clone(),
                        panes.clone(),
                        def.clone(),
                    );
                    drop(popup_menu(items));
                })
                .child(
                    text("派发")
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground),
                )
                .child(
                    icon_svg("icons/chevron-down.svg", 11.0)
                        .text_color(move || active_theme().muted_foreground),
                )
                .into_element(),
        )
    } else {
        None
    };

    let open_detail = ctx.open_detail;
    let drag = ctx.drag;
    let more_item = item.clone();
    let card_item = item.clone();
    let tx_jump = ctx.tx.clone();
    let drag_title = item.title.clone();

    let ctx_tx = ctx.tx.clone();
    let ctx_item = item.clone();
    let ctx_installed = ctx.snap.installed_agents.clone();
    let ctx_panes = ctx.snap.live_panes.clone();
    let ctx_default = ctx.snap.default_agent.clone();

    let mut body = view()
        .flex_col()
        .w_full()
        .py_2()
        .pr_2p5()
        .pl(px(if accent_kind.is_some() { 9.0 } else { 10.0 }))
        .gap_2()
        .child(
            view()
                .flex_row()
                .w_full()
                .gap_1p5()
                .items_start()
                .child(
                    text(item.title.clone())
                        .flex_1()
                        .min_w_0()
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || active_theme().foreground),
                )
                .child(
                    view()
                        .role(AccessibilityRole::Button)
                        .label("查看详情")
                        .p_0p5()
                        .rounded(move || active_theme().radius)
                        .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                        .on_press(move || open_detail(more_item.clone()))
                        .child(
                            icon_svg("icons/ellipsis.svg", 13.0)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                ),
        );

    if let Some(badge) = render_source_badge(ctx.tx, item) {
        body = body.child(view().flex_row().items_center().child(badge));
    }

    body = body.child({
        let mut footer = view()
            .flex_row()
            .w_full()
            .gap_1p5()
            .items_center()
            .child(pill_el)
            .child(view().flex_1());
        if let Some(cta) = right_cta {
            footer = footer.child(cta);
        }
        footer
    });

    if let Some(kind) = accent_kind {
        body = body.border_l(px(3.0)).border_l_color(move || match kind {
            0 => warning_color(),
            1 => success_color(),
            _ => active_theme().destructive,
        });
    }

    view()
        .role(AccessibilityRole::Button)
        .w_full()
        .rounded(move || active_theme().radius)
        .bg(move || active_theme().background)
        .border_1()
        .border_color(move || active_theme().border)
        .hover(|s| s.border_color(move || link_color().opacity(0.45)))
        .overflow_hidden()
        .on_press_down(move || {
            drag.candidate.set(Some(DragCandidate {
                id,
                title: drag_title.clone(),
                start_y: None,
            }));
        })
        .on_press(move || {
            if drag.active.get_untracked().is_some() {
                return;
            }
            if let Some(pane) = jump_pane {
                send_action(&tx_jump, TodoAction::RevealPane(pane));
            } else {
                open_detail(card_item.clone());
            }
        })
        .on_context_menu(move |ev: &ContextMenuEvent| {
            let items = build_card_context_menu_items(CardContextMenuArgs {
                tx: ctx_tx.clone(),
                open_detail,
                item: ctx_item.clone(),
                jump_pane,
                installed_agents: ctx_installed.clone(),
                live_panes: ctx_panes.clone(),
                default_agent: ctx_default.clone(),
            });
            drop(popup_menu_at(items, ev.page_position));
        })
        .child(body)
        .into_element()
}

pub(super) fn render_todo_card<F>(
    ctx: &CardContext<'_, F>,
    item: &TodoItem,
    default_label: &str,
) -> AnyElement
where
    F: Fn(TodoItem) + Copy + 'static,
{
    let id = item.id;
    let drag = ctx.drag;
    let open_detail = ctx.open_detail;
    let drag_title = item.title.clone();
    let open_item = item.clone();
    let more_item = item.clone();
    let tx_dispatch = ctx.tx.clone();
    let tx_menu = ctx.tx.clone();
    let tx_done = ctx.tx.clone();

    let menu_item = item.clone();
    let menu_installed = ctx.snap.installed_agents.clone();
    let menu_panes = ctx.snap.live_panes.clone();
    let menu_default = ctx.snap.default_agent.clone();

    let ctx_tx = ctx.tx.clone();
    let ctx_item = item.clone();
    let ctx_installed = ctx.snap.installed_agents.clone();
    let ctx_panes = ctx.snap.live_panes.clone();
    let ctx_default = ctx.snap.default_agent.clone();

    let desc_preview = item
        .description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string);

    let split_btn = view()
        .flex_row()
        .flex_shrink_0()
        .h(px(22.0))
        .rounded(px(5.0))
        .bg(move || active_theme().sidebar)
        .overflow_hidden()
        .items_center()
        .child(
            view()
                .role(AccessibilityRole::Button)
                .h_full()
                .px_2()
                .gap_1()
                .flex_row()
                .items_center()
                .hover(|s| s.bg(move || active_theme().accent.opacity(0.75)))
                .on_press(move || {
                    send_action(&tx_dispatch, TodoAction::DispatchDefault(id));
                })
                .child(icon_svg("icons/play.svg", 10.0).text_color(link_color))
                .child(
                    text(format!("交给 {default_label}"))
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || active_theme().foreground),
                ),
        )
        .child(
            view()
                .w(px(1.0))
                .h(px(14.0))
                .bg(move || active_theme().sidebar_border),
        )
        .child(
            view()
                .role(AccessibilityRole::Button)
                .label("选择 Agent 或关联已有窗格")
                .h_full()
                .px_1p5()
                .items_center()
                .justify_center()
                .hover(|s| s.bg(move || active_theme().accent.opacity(0.75)))
                .on_press(move || {
                    let items = build_dispatch_menu_items(
                        tx_menu.clone(),
                        menu_item.clone(),
                        menu_installed.clone(),
                        menu_panes.clone(),
                        menu_default.clone(),
                    );
                    drop(popup_menu(items));
                })
                .child(
                    icon_svg("icons/chevron-down.svg", 11.0)
                        .text_color(move || active_theme().muted_foreground),
                ),
        );

    let quick_done = view()
        .role(AccessibilityRole::Button)
        .label("标记为已完成")
        .flex_shrink_0()
        .p_1()
        .rounded(px(5.0))
        .hover(|s| s.bg(move || success_color().opacity(0.16)))
        .on_press(move || {
            send_action(
                &tx_done,
                TodoAction::SetStatus {
                    id,
                    status: TodoStatus::Done,
                },
            );
        })
        .child(
            icon_svg("icons/check.svg", 13.0).text_color(move || active_theme().muted_foreground),
        );

    let mut card = view()
        .role(AccessibilityRole::Button)
        .flex_col()
        .w_full()
        .px_2p5()
        .py_2()
        .gap_1p5()
        .rounded(move || active_theme().radius)
        .bg(move || active_theme().background)
        .border_1()
        .border_color(move || active_theme().border)
        .hover(|s| s.border_color(move || link_color().opacity(0.45)))
        .on_press_down(move || {
            drag.candidate.set(Some(DragCandidate {
                id,
                title: drag_title.clone(),
                start_y: None,
            }));
        })
        .on_press(move || {
            if drag.active.get_untracked().is_some() {
                return;
            }
            open_detail(open_item.clone());
        })
        .on_context_menu(move |ev: &ContextMenuEvent| {
            let items = build_card_context_menu_items(CardContextMenuArgs {
                tx: ctx_tx.clone(),
                open_detail,
                item: ctx_item.clone(),
                jump_pane: None,
                installed_agents: ctx_installed.clone(),
                live_panes: ctx_panes.clone(),
                default_agent: ctx_default.clone(),
            });
            drop(popup_menu_at(items, ev.page_position));
        })
        .child(
            view()
                .flex_row()
                .w_full()
                .gap_1p5()
                .items_start()
                .child(
                    text(item.title.clone())
                        .flex_1()
                        .min_w_0()
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || active_theme().foreground),
                )
                .child(
                    view()
                        .role(AccessibilityRole::Button)
                        .label("查看详情")
                        .p_0p5()
                        .rounded(move || active_theme().radius)
                        .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                        .on_press(move || open_detail(more_item.clone()))
                        .child(
                            icon_svg("icons/ellipsis.svg", 13.0)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                ),
        );

    if let Some(badge) = render_source_badge(ctx.tx, item) {
        card = card.child(view().flex_row().items_center().child(badge));
    }

    if let Some(preview) = desc_preview {
        card = card.child(meta_text(preview));
    }

    card.child(
        view()
            .flex_row()
            .w_full()
            .pt_0p5()
            .gap_1p5()
            .items_center()
            .child(split_btn)
            .child(view().flex_1())
            .child(quick_done),
    )
    .into_element()
}

pub(super) fn render_done_row<F>(ctx: &CardContext<'_, F>, item: &TodoItem) -> AnyElement
where
    F: Fn(TodoItem) + Copy + 'static,
{
    let id = item.id;
    let drag = ctx.drag;
    let open_detail = ctx.open_detail;
    let drag_title = item.title.clone();
    let open_item = item.clone();
    let ctx_tx = ctx.tx.clone();
    let ctx_item = item.clone();

    let mut row = view()
        .role(AccessibilityRole::Button)
        .flex_row()
        .w_full()
        .h(px(28.0))
        .px_1p5()
        .gap_1p5()
        .rounded(move || active_theme().radius)
        .items_center()
        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
        .on_press_down(move || {
            drag.candidate.set(Some(DragCandidate {
                id,
                title: drag_title.clone(),
                start_y: None,
            }));
        })
        .on_press(move || {
            if drag.active.get_untracked().is_some() {
                return;
            }
            open_detail(open_item.clone());
        })
        .on_context_menu(move |ev: &ContextMenuEvent| {
            let items = build_card_context_menu_items(CardContextMenuArgs {
                tx: ctx_tx.clone(),
                open_detail,
                item: ctx_item.clone(),
                jump_pane: None,
                installed_agents: Vec::new(),
                live_panes: Vec::new(),
                default_agent: None,
            });
            drop(popup_menu_at(items, ev.page_position));
        })
        .child(icon_svg("icons/check.svg", 12.0).text_color(success_color))
        .child(
            text(item.title.clone())
                .flex_1()
                .min_w_0()
                .text_size(text_xs_size)
                .text_color(move || active_theme().muted_foreground)
                .line_through(),
        );
    if let Some(badge) = render_source_badge(ctx.tx, item) {
        row = row.child(badge);
    }
    if let Some(agent) = item.agent_id.as_deref() {
        row = row.child(meta_text(short_agent_name(agent)));
    }
    row.into_element()
}
