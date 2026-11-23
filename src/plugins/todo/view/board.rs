//! Main Kanban board layout with collapsible lanes and drag-over drop targets.

use std::collections::HashMap;

use futures::channel::mpsc::UnboundedSender;
use nsh_base::theme as active_theme;
use nsh_core::MouseEvent;
use nsh_core::element::{AccessibilityRole, scroll};
use nsh_core::prelude::*;

use super::cards::{CardContext, render_done_row, render_in_progress_card, render_todo_card};
use super::composer::{ComposerSignals, render_composer_section};
use super::{DragState, send_action};
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoStatus};
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::meta_text;
use crate::nsh_ui::theme::{link_color, text_xs_size};
use crate::plugins::AgentPaneInfo;
use crate::plugins::todo::{TodoAction, TodoBoardSnapshot, short_agent_name};

#[derive(Clone, Copy)]
pub(super) struct LaneSignals {
    pub in_progress_open: Signal<bool>,
    pub todo_open: Signal<bool>,
    pub done_open: Signal<bool>,
}

pub(super) struct BoardContext<F> {
    pub tx: Option<UnboundedSender<TodoAction>>,
    pub composer: ComposerSignals,
    pub lanes: LaneSignals,
    pub drag: DragState,
    pub open_detail: F,
}

pub(super) fn render_board_view<F>(ctx: BoardContext<F>, snap: TodoBoardSnapshot) -> AnyElement
where
    F: Fn(TodoItem) + Copy + 'static,
{
    let pane_map: HashMap<u64, AgentPaneInfo> = snap
        .live_panes
        .iter()
        .cloned()
        .map(|p| (p.pane, p))
        .collect();

    let mut in_progress: Vec<TodoItem> = Vec::new();
    let mut todo_items: Vec<TodoItem> = Vec::new();
    let mut done_items: Vec<TodoItem> = Vec::new();
    for item in &snap.todos {
        match item.status {
            TodoStatus::InProgress => in_progress.push(item.clone()),
            TodoStatus::Todo => todo_items.push(item.clone()),
            TodoStatus::Done => done_items.push(item.clone()),
        }
    }
    in_progress.sort_by_key(|item| {
        let state = item
            .pane_id
            .and_then(|p| pane_map.get(&p))
            .and_then(|info| info.state);
        let rank = if state == Some(AgentState::Waiting) {
            0
        } else if item.awaiting_review || state == Some(AgentState::Completed) {
            1
        } else if state == Some(AgentState::Failed) {
            2
        } else if state == Some(AgentState::Running) {
            3
        } else {
            4
        };
        (rank, item.position, std::cmp::Reverse(item.id))
    });

    let default_label = snap
        .default_agent
        .as_deref()
        .map(short_agent_name)
        .unwrap_or_else(|| "Agent".to_string());

    let quick_wrap = render_composer_section(
        ctx.tx.clone(),
        ctx.composer,
        snap.shared_board_name.as_deref(),
        default_label.clone(),
    );

    let card_ctx = CardContext {
        tx: &ctx.tx,
        snap: &snap,
        drag: ctx.drag,
        open_detail: ctx.open_detail,
    };

    let in_progress_cards: Vec<AnyElement> = in_progress
        .iter()
        .map(|item| render_in_progress_card(&card_ctx, item, &pane_map))
        .collect();
    let todo_cards: Vec<AnyElement> = todo_items
        .iter()
        .map(|item| render_todo_card(&card_ctx, item, &default_label))
        .collect();
    let done_rows: Vec<AnyElement> = done_items
        .iter()
        .map(|item| render_done_row(&card_ctx, item))
        .collect();

    let ip_hdr = render_lane_header(
        &ctx.tx,
        TodoStatus::InProgress,
        in_progress.len(),
        ctx.lanes.in_progress_open,
    );
    let todo_hdr = render_lane_header(
        &ctx.tx,
        TodoStatus::Todo,
        todo_items.len(),
        ctx.lanes.todo_open,
    );
    let done_hdr = render_lane_header(
        &ctx.tx,
        TodoStatus::Done,
        done_items.len(),
        ctx.lanes.done_open,
    );

    let body = scroll().flex_1().child(
        view()
            .flex_col()
            .w_full()
            .px_2()
            .py_2()
            .gap_2()
            .child(render_lane(
                ctx.drag,
                TodoStatus::InProgress,
                ip_hdr,
                in_progress_cards,
                ctx.lanes.in_progress_open.get(),
                "暂无进行中的任务",
            ))
            .child(lane_divider())
            .child(render_lane(
                ctx.drag,
                TodoStatus::Todo,
                todo_hdr,
                todo_cards,
                ctx.lanes.todo_open.get(),
                "在上方输入框添加待办",
            ))
            .child(lane_divider())
            .child(render_lane(
                ctx.drag,
                TodoStatus::Done,
                done_hdr,
                done_rows,
                ctx.lanes.done_open.get(),
                "还没有已完成的任务",
            )),
    );

    view()
        .flex_col()
        .flex_1()
        .w_full()
        .h_full()
        .child(quick_wrap)
        .child(body)
        .into_element()
}

fn lane_divider() -> View {
    view()
        .w_full()
        .h(px(1.0))
        .flex_shrink_0()
        .bg(move || active_theme().sidebar_border)
}

fn render_lane_header(
    tx: &Option<UnboundedSender<TodoAction>>,
    status: TodoStatus,
    count: usize,
    open_sig: Signal<bool>,
) -> View {
    let expanded = open_sig.get();
    let mut hdr = view()
        .role(AccessibilityRole::Button)
        .w_full()
        .h(px(26.0))
        .px_1()
        .gap_1p5()
        .rounded(move || active_theme().radius)
        .flex_row()
        .items_center()
        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
        .on_press(move || open_sig.update(|v| *v = !*v))
        .child(
            text(status.label())
                .text_size(text_xs_size)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || active_theme().muted_foreground),
        )
        .child(view().flex_1());

    if status == TodoStatus::Done && count > 0 {
        let tx_clear = tx.clone();
        hdr = hdr.child(
            view()
                .role(AccessibilityRole::Button)
                .px_1p5()
                .py_0p5()
                .rounded(move || active_theme().radius)
                .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
                .on_press(move || send_action(&tx_clear, TodoAction::ClearDone))
                .child(meta_text("清空")),
        );
    }

    hdr.child(
        text(count.to_string())
            .text_size(text_xs_size)
            .font_weight(FontWeight::MEDIUM)
            .text_color(move || active_theme().muted_foreground),
    )
    .child(
        icon_svg(
            if expanded {
                "icons/chevron-down.svg"
            } else {
                "icons/chevron-right.svg"
            },
            12.0,
        )
        .text_color(move || active_theme().muted_foreground),
    )
}

fn render_lane(
    drag: DragState,
    target: TodoStatus,
    header: View,
    cards: Vec<AnyElement>,
    expanded: bool,
    empty_hint: &'static str,
) -> View {
    let mut lane = view()
        .flex_col()
        .w_full()
        .gap_1p5()
        .p_0p5()
        .rounded(move || active_theme().radius)
        .style_when(
            move || drag.active.get().is_some() && drag.over_lane.get() == Some(target),
            |s| {
                s.bg(move || link_color().opacity(0.08))
                    .border_1()
                    .border_color(move || link_color().opacity(0.45))
            },
        )
        .on_mouse_enter(move |_: &MouseEvent| {
            if drag.active.get_untracked().is_some() {
                drag.over_lane.set(Some(target));
            }
        })
        .child(header);
    if expanded {
        if cards.is_empty() {
            lane = lane.child(view().px_1p5().py_1().child(meta_text(empty_hint)));
        } else {
            for card in cards {
                lane = lane.child(card);
            }
        }
    }
    lane
}
