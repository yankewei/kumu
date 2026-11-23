//! Pure Rust NSH UI for the right-hand TODO Kanban board and inline detail editor.
//!
//! Zero GPUI imports: state flows in from the host via [`TodoHostEvent`] and
//! user intents flow back via [`TodoAction`].

mod board;
mod cards;
mod composer;
mod detail;
mod menus;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use nsh_base::theme as active_theme;
use nsh_core::element::{AccessibilityRole, SignalEquals, signal_with};
use nsh_core::prelude::*;
use nsh_core::{MouseButton, MouseEvent};

use super::{TodoAction, TodoBoardSnapshot, TodoHostEvent, take_mount};
use crate::db::{TodoItem, TodoStatus};
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::theme::{link_color, text_xs_size};

use self::board::{BoardContext, LaneSignals, render_board_view};
use self::composer::ComposerSignals;
use self::detail::{DetailSignals, render_detail_view};

#[derive(Clone, Debug, PartialEq, Props)]
pub(super) struct TodoRootProps {
    pub(super) mount_id: Prop<u64>,
}

#[derive(Clone, Debug, PartialEq)]
struct DragCandidate {
    id: i64,
    title: String,
    start_y: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
struct ActiveDrag {
    id: i64,
    title: String,
    y: f32,
}

#[derive(Clone, Copy)]
struct DragState {
    candidate: Signal<Option<DragCandidate>>,
    active: Signal<Option<ActiveDrag>>,
    over_lane: Signal<Option<TodoStatus>>,
}

fn send_action(tx: &Option<UnboundedSender<TodoAction>>, action: TodoAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

fn render_notice_banner(notice: Signal<Option<(String, bool)>>) -> impl IntoElement {
    dynamic(move || {
        let Some((message, is_error)) = notice.get() else {
            return view().into_element();
        };
        let color = move || {
            if is_error {
                active_theme().destructive
            } else {
                active_theme().muted_foreground
            }
        };
        view()
            .flex_row()
            .px_2()
            .py_1()
            .gap_1p5()
            .rounded(move || active_theme().radius)
            .bg(move || color().opacity(0.12))
            .items_center()
            .child(
                text(message)
                    .flex_1()
                    .min_w_0()
                    .text_size(text_xs_size)
                    .text_color(color),
            )
            .child(
                view()
                    .role(AccessibilityRole::Button)
                    .label("关闭提示")
                    .on_press(move || notice.set(None))
                    .child(icon_svg("icons/x.svg", 11.0).text_color(color)),
            )
            .into_element()
    })
}

#[component]
pub(super) fn TodoRoot(props: TodoRootProps) -> impl IntoElement {
    let mount = take_mount(props.mount_id.get_untracked());
    let (initial, events_rx, actions_tx) = match mount {
        Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
        None => (TodoBoardSnapshot::default(), None, None),
    };

    let board = signal_with(initial, SignalEquals::Never);
    let notice = signal(Option::<(String, bool)>::None);

    let composer = ComposerSignals {
        notice,
        quick_draft: signal(String::new()),
        composer_open: signal(false),
        composer_title: signal(String::new()),
        composer_desc: signal(String::new()),
    };

    let lanes = LaneSignals {
        in_progress_open: signal(true),
        todo_open: signal(true),
        done_open: signal(false),
    };

    // Inline detail editor state (`← 返回看板`).
    let detail = DetailSignals {
        notice,
        id: signal(Option::<i64>::None),
        title: signal(String::new()),
        desc: signal(String::new()),
        followup: signal(String::new()),
    };

    // Lightweight mouse drag state across lanes (never interferes with wheel scroll).
    let drag = DragState {
        candidate: signal(Option::<DragCandidate>::None),
        active: signal(Option::<ActiveDrag>::None),
        over_lane: signal(Option::<TodoStatus>::None),
    };

    let open_detail = move |item: TodoItem| {
        detail.notice.set(None);
        detail.title.set(item.title);
        detail.desc.set(item.description);
        detail.followup.set(String::new());
        detail.id.set(Some(item.id));
    };

    if let Some(mut rx) = events_rx {
        spawn(async move {
            while let Some(event) = rx.next().await {
                match event {
                    TodoHostEvent::Snapshot(next) => {
                        let prev_board = board.get_untracked().board_id;
                        let next_board = next.board_id;
                        if prev_board != next_board {
                            detail.id.set(None);
                            notice.set(None);
                        } else if let Some(id) = detail.id.get_untracked()
                            && !next.todos.iter().any(|t| t.id == id)
                        {
                            detail.id.set(None);
                        }
                        board.set(next);
                    }
                    TodoHostEvent::Notice { message, is_error } => {
                        notice.set(Some((message, is_error)));
                    }
                    TodoHostEvent::ClearNotice => {
                        notice.set(None);
                    }
                    TodoHostEvent::OpenInProgressLane => {
                        lanes.in_progress_open.set(true);
                    }
                    TodoHostEvent::ClearFollowup => {
                        detail.followup.set(String::new());
                    }
                }
            }
        });
    }

    let tx_content = actions_tx.clone();
    let tx_drop = actions_tx;

    let content = dynamic(move || {
        let snap = board.get();
        if !snap.has_project {
            return view()
                .flex_1()
                .px_3()
                .py_4()
                .child(crate::nsh_ui::components::meta_text("没有选中的项目"))
                .into_element();
        }

        if let Some(id) = detail.id.get()
            && let Some(item) = snap.todos.iter().find(|t| t.id == id).cloned()
        {
            return render_detail_view(tx_content.clone(), detail, item, snap).into_element();
        }

        render_board_view(
            BoardContext {
                tx: tx_content.clone(),
                composer,
                lanes,
                drag,
                open_detail,
            },
            snap,
        )
    });

    // Floating drag preview chip
    let drag_chip = dynamic(move || {
        let Some(active) = drag.active.get() else {
            return view().into_element();
        };
        view()
            .absolute()
            .left(px(12.0))
            .top(px((active.y - 16.0).max(8.0)))
            .max_w(px(220.0))
            .px_2p5()
            .py_1p5()
            .rounded(move || active_theme().radius)
            .bg(move || active_theme().background)
            .border_1()
            .border_color(link_color)
            .pointer_events(PointerEvents::None)
            .child(
                text(active.title)
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().foreground),
            )
            .into_element()
    });

    view()
        .relative()
        .flex_col()
        .flex_1()
        .w_full()
        .h_full()
        .bg(move || active_theme().sidebar)
        .font_family(move || {
            active_theme()
                .font_sans
                .clone()
                .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
        })
        .on_mouse_move(move |ev: &MouseEvent| {
            if ev.pressed_button != Some(MouseButton::Left) {
                return;
            }
            if let Some(mut cur) = drag.active.get_untracked() {
                cur.y = ev.page_position.y.0;
                drag.active.set(Some(cur));
            } else if let Some(mut cand) = drag.candidate.get_untracked() {
                let y = ev.page_position.y.0;
                match cand.start_y {
                    None => {
                        cand.start_y = Some(y);
                        drag.candidate.set(Some(cand));
                    }
                    Some(start) if (y - start).abs() > 6.0 => {
                        drag.active.set(Some(ActiveDrag {
                            id: cand.id,
                            title: cand.title,
                            y,
                        }));
                    }
                    _ => {}
                }
            }
        })
        .on_mouse_up(move |_: &MouseEvent| {
            if let Some(active) = drag.active.get_untracked()
                && let Some(status) = drag.over_lane.get_untracked()
            {
                send_action(
                    &tx_drop,
                    TodoAction::SetStatus {
                        id: active.id,
                        status,
                    },
                );
            }
            drag.candidate.set(None);
            drag.active.set(None);
            drag.over_lane.set(None);
        })
        .child(content)
        .child(drag_chip)
}
