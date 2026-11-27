//! Pure Rust NSH 3-column Jira-style TODO Kanban board (`待办` / `进行中` / `已完成`).
//!
//! Features:
//! - Minimalist cards with title, bottom-left `#ID`, and bottom-right
//!   left-over-right overlapping circular Agent avatar stack (max 3 + `+N`).
//! - Pointer-based cross-column drag & drop with a `6px` movement threshold so
//!   clicks cleanly open the full-window detail modal.
//! - Top toolbar with search, filter chips (`全部` / `已关联` / `需关注`),
//!   `＋ 新建任务`, and `返回终端`.

use std::collections::HashMap;

use futures::channel::mpsc::UnboundedSender;
use nsh_base::theme as active_theme;
use nsh_core::element::scroll;
use nsh_core::prelude::*;

use super::{
    TodoBoardAction, TodoBoardFilter, TodoBoardSnapshot, TodoCardSnapshot, TodoLinkSnapshot,
};
use crate::agent::AgentState;
use crate::db::TodoStatus;
use crate::nsh_ui::assets::{agent_logo, icon_svg};
use crate::nsh_ui::components::{
    ButtonVariant, ControlTone, body_text, kumu_button, kumu_icon_button, kumu_input, kumu_tooltip,
    meta_text,
};
use crate::nsh_ui::theme::{link_color, success_color, text_xs_size, warning_color};

const DRAG_THRESHOLD_PX: f32 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct DragSession {
    todo_id: i64,
    from_status: TodoStatus,
    start_x: f32,
    start_y: f32,
    active: bool,
    hover_status: Option<TodoStatus>,
}

fn send(tx: &Option<UnboundedSender<TodoBoardAction>>, action: TodoBoardAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

fn render_avatar_stack(links: &[TodoLinkSnapshot]) -> AnyElement {
    if links.is_empty() {
        return view().into_element();
    }
    let shown = &links[..links.len().min(3)];
    let overflow = links.len().saturating_sub(3);

    let mut row = view().flex_row().items_center().flex_shrink_0();
    for (idx, link) in shown.iter().enumerate() {
        let is_live = link.pane_id.is_some();
        let state = link.state;
        let awaiting = link.awaiting_review;
        let z = (10 - idx as i32).max(2);
        let tip = {
            let mut s = link.agent_name.clone();
            if let Some(ses) = &link.session_id {
                s.push_str(" · ");
                s.push_str(ses);
            }
            if is_live {
                let st = if state == Some(AgentState::Waiting) {
                    "等待输入"
                } else if awaiting {
                    "待确认"
                } else if state == Some(AgentState::Failed) {
                    "执行失败"
                } else if state == Some(AgentState::Running) {
                    "运行中"
                } else {
                    "在线"
                };
                s.push_str(&format!("（{st}）"));
            } else {
                s.push_str("（历史会话）");
            }
            s
        };

        let icon_el: AnyElement = if link.agent_id.is_none() {
            icon_svg("icons/square-terminal.svg", 12.0)
                .text_color(move || active_theme().muted_foreground)
                .into_element()
        } else {
            agent_logo(link.logo, 12.0)
        };

        let mut circle = view()
            .relative()
            .w(px(24.0))
            .h(px(24.0))
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(12.0))
            .bg(move || active_theme().background)
            .border(px(2.0))
            .border_color(move || active_theme().card)
            .z_index(z)
            .tooltip(kumu_tooltip(tip))
            .child(icon_el);

        if idx > 0 {
            circle = circle.ml(px(-7.0));
        }
        if !is_live {
            circle = circle.opacity(0.55);
        }

        if is_live {
            let dot_color = if state == Some(AgentState::Waiting) || awaiting {
                Some(warning_color as fn() -> AdaptiveColor)
            } else if state == Some(AgentState::Failed) {
                Some((|| active_theme().destructive) as fn() -> AdaptiveColor)
            } else if state == Some(AgentState::Running) {
                Some(link_color as fn() -> AdaptiveColor)
            } else {
                None
            };
            if let Some(dot_bg) = dot_color {
                circle = circle.child(
                    view()
                        .absolute()
                        .right(px(-1.0))
                        .bottom(px(-1.0))
                        .w(px(7.0))
                        .h(px(7.0))
                        .rounded(px(3.5))
                        .bg(dot_bg)
                        .border(px(1.5))
                        .border_color(move || active_theme().card),
                );
            }
        }

        row = row.child(circle);
    }

    if overflow > 0 {
        row = row.child(
            view()
                .relative()
                .min_w(px(24.0))
                .h(px(24.0))
                .px_1()
                .ml(px(-7.0))
                .z_index(1)
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(12.0))
                .bg(move || active_theme().muted)
                .border(px(2.0))
                .border_color(move || active_theme().card)
                .tooltip(kumu_tooltip(format!("还有 {overflow} 个关联会话")))
                .child(
                    text(format!("+{overflow}"))
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || active_theme().foreground),
                ),
        );
    }

    row.into_element()
}

fn render_card(
    card: &TodoCardSnapshot,
    drag_sig: Signal<Option<DragSession>>,
    suppress_click_todo: Stored<Option<i64>>,
    tx: Option<UnboundedSender<TodoBoardAction>>,
) -> AnyElement {
    let todo_id = card.id;
    let status = card.status;
    let is_done = status == TodoStatus::Done;
    let needs_attn = card.needs_attention;
    let tx_click = tx.clone();

    let mut el = view()
        .role(nsh_core::element::AccessibilityRole::Button)
        .label(card.title.clone())
        .flex_col()
        .w_full()
        .p_3()
        .gap_2p5()
        .rounded(move || active_theme().radius)
        .bg(move || active_theme().card)
        .border_1()
        .border_color(move || {
            if drag_sig
                .get()
                .is_some_and(|d| d.active && d.todo_id == todo_id)
            {
                link_color()
            } else {
                active_theme().border
            }
        })
        .hover(|s| s.border_color(move || active_theme().muted_foreground.opacity(0.6)))
        .on_press_down(move |ev: &nsh_core::PressEvent| {
            suppress_click_todo.replace(None);
            drag_sig.set(Some(DragSession {
                todo_id,
                from_status: status,
                start_x: ev.page_position.x.0,
                start_y: ev.page_position.y.0,
                active: false,
                hover_status: Some(status),
            }));
        })
        .on_press(move || {
            if suppress_click_todo.get() == Some(todo_id) {
                suppress_click_todo.replace(None);
                return;
            }
            if let Some(d) = drag_sig.get_untracked()
                && d.todo_id == todo_id
                && d.active
            {
                return;
            }
            drag_sig.set(None);
            send(&tx_click, TodoBoardAction::OpenDetailModal(todo_id));
        })
        .child(
            text(card.title.clone())
                .w_full()
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || {
                    if is_done {
                        active_theme().muted_foreground
                    } else {
                        active_theme().foreground
                    }
                }),
        )
        .child(
            view()
                .flex_row()
                .w_full()
                .min_h(px(24.0))
                .items_center()
                .justify_between()
                .gap_2()
                .child(meta_text(format!("#{todo_id}")))
                .child(render_avatar_stack(&card.links)),
        );

    if needs_attn {
        el = el.border_l(px(3.0)).border_l_color(warning_color);
    }
    if is_done {
        el = el.opacity(0.72);
    }
    el.into_element()
}

pub(in crate::ui::home) fn render_todo_board(
    snap_sig: Signal<Option<TodoBoardSnapshot>>,
    actions_tx: Option<UnboundedSender<TodoBoardAction>>,
) -> impl IntoElement {
    let search_query = signal(String::new());
    let filter_mode = signal(TodoBoardFilter::All);
    let drag_sig = signal(Option::<DragSession>::None);
    let suppress_click_todo = stored(Option::<i64>::None);
    let search_ref = element_ref();
    let last_search_nonce = stored(0_u64);
    let last_board_id = stored(Option::<i64>::None);

    let tx_root = actions_tx;
    dynamic(move || {
        let Some(snap) = snap_sig.get() else {
            return view().size_full().into_element();
        };

        if last_board_id.get() != Some(snap.board_id) {
            let switched = last_board_id.get().is_some();
            last_board_id.replace(Some(snap.board_id));
            if switched {
                if !search_query.get_untracked().is_empty() {
                    search_query.set(String::new());
                }
                if filter_mode.get_untracked() != TodoBoardFilter::All {
                    filter_mode.set(TodoBoardFilter::All);
                }
                if drag_sig.get_untracked().is_some() {
                    drag_sig.set(None);
                }
            }
        }

        if snap.search_focus_nonce != last_search_nonce.get() {
            last_search_nonce.replace(snap.search_focus_nonce);
            search_ref.focus();
        }

        let query = search_query.get().trim().to_lowercase();
        let active_filter = filter_mode.get();

        let filtered: Vec<&TodoCardSnapshot> = snap
            .todos
            .iter()
            .filter(|t| {
                if !query.is_empty() {
                    let id_str = format!("#{}", t.id);
                    if !t.title.to_lowercase().contains(&query)
                        && !t.description.to_lowercase().contains(&query)
                        && !id_str.contains(&query)
                    {
                        return false;
                    }
                }
                match active_filter {
                    TodoBoardFilter::All => true,
                    TodoBoardFilter::Linked => !t.links.is_empty(),
                    TodoBoardFilter::Attention => t.needs_attention,
                }
            })
            .collect();

        let mut by_col: HashMap<TodoStatus, Vec<&TodoCardSnapshot>> = HashMap::new();
        for card in filtered {
            by_col.entry(card.status).or_default().push(card);
        }

        let tx_new = tx_root.clone();
        let tx_back = tx_root.clone();
        let tx_clear = tx_root.clone();
        let done_count = by_col.get(&TodoStatus::Done).map_or(0, Vec::len);

        let filter_chip = |label: &'static str, mode: TodoBoardFilter| {
            let selected = active_filter == mode;
            nsh_base::button()
                .h(px(26.0))
                .px_2p5()
                .rounded(move || active_theme().radius)
                .items_center()
                .justify_center()
                .bg(move || {
                    if selected {
                        link_color().opacity(0.14)
                    } else {
                        AdaptiveColor::TRANSPARENT
                    }
                })
                .hover(|s| s.bg(move || active_theme().accent))
                .on_press(move || filter_mode.set(mode))
                .child(
                    text(label)
                        .text_size(text_xs_size)
                        .font_weight(if selected {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(move || {
                            if selected {
                                link_color()
                            } else {
                                active_theme().muted_foreground
                            }
                        }),
                )
        };

        let header = view()
            .flex_row()
            .flex_shrink_0()
            .w_full()
            .px_4()
            .py_2p5()
            .gap_3()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_b_color(move || active_theme().border)
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        icon_svg("icons/folder-open.svg", 14.0)
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .child(
                        body_text(format!("{} · 看板", snap.board_name))
                            .font_weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .flex_shrink_0()
                    .child(
                        kumu_input("搜索标题或 #编号…")
                            .element_ref(&search_ref)
                            .w(px(176.0))
                            .h(px(26.0))
                            .value(move || search_query.get())
                            .on_change(move |val: &str| {
                                search_query.set(val.to_owned());
                            }),
                    )
                    .child(filter_chip("全部", TodoBoardFilter::All))
                    .child(filter_chip("已关联", TodoBoardFilter::Linked))
                    .child(filter_chip("需关注", TodoBoardFilter::Attention))
                    .child(
                        kumu_button("＋ 新建任务", ButtonVariant::Primary).on_press(move || {
                            send(&tx_new, TodoBoardAction::OpenCreateModal(TodoStatus::Todo));
                        }),
                    )
                    .child(
                        kumu_button("返回终端", ButtonVariant::Ghost).on_press(move || {
                            send(&tx_back, TodoBoardAction::BackToTerminal);
                        }),
                    ),
            );

        let columns = [
            (TodoStatus::Todo, TodoStatus::Todo.label()),
            (TodoStatus::InProgress, TodoStatus::InProgress.label()),
            (TodoStatus::Done, TodoStatus::Done.label()),
        ];

        let mut grid = view().flex_row().flex_1().min_h_0().min_w_0().p_4().gap_3();

        for (status, title) in columns {
            let cards = by_col.get(&status).cloned().unwrap_or_default();
            let count = cards.len();
            let tx_col_add = tx_root.clone();
            let tx_col_add_bottom = tx_root.clone();
            let tx_clear_col = tx_clear.clone();
            let tx_col_drop = tx_root.clone();

            let dot_el: AnyElement = match status {
                TodoStatus::Todo => view()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded(px(3.5))
                    .border(px(1.5))
                    .border_color(move || active_theme().muted_foreground)
                    .into_element(),
                TodoStatus::InProgress => view()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded(px(3.5))
                    .bg(link_color)
                    .into_element(),
                TodoStatus::Done => view()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded(px(3.5))
                    .bg(success_color)
                    .into_element(),
            };

            let mut col_hdr = view()
                .flex_row()
                .flex_shrink_0()
                .w_full()
                .px_3()
                .py_2()
                .items_center()
                .justify_between()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(dot_el)
                        .child(
                            text(title)
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || active_theme().foreground),
                        )
                        .child(
                            view()
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(999.0))
                                .bg(move || active_theme().muted)
                                .child(meta_text(count.to_string())),
                        ),
                );

            let mut hdr_right = view().flex_row().items_center().gap_1();
            if status == TodoStatus::Done && done_count > 0 {
                hdr_right = hdr_right.child(kumu_button("清空", ButtonVariant::Ghost).on_press(
                    move || {
                        send(&tx_clear_col, TodoBoardAction::ClearDone);
                    },
                ));
            }
            hdr_right = hdr_right.child(kumu_icon_button(
                "在此列新建任务",
                icon_svg("icons/plus.svg", 13.0)
                    .text_color(move || active_theme().muted_foreground),
                ControlTone::Default,
                move || send(&tx_col_add, TodoBoardAction::OpenCreateModal(status)),
            ));
            col_hdr = col_hdr.child(hdr_right);

            let mut list_col = view().flex_col().w_full().px_2().pb_2().gap_2();
            if cards.is_empty() {
                list_col = list_col.child(
                    view()
                        .w_full()
                        .py_6()
                        .items_center()
                        .justify_center()
                        .child(meta_text("拖入卡片或点击下方新建")),
                );
            } else {
                for card in cards {
                    list_col = list_col.child(render_card(
                        card,
                        drag_sig,
                        suppress_click_todo,
                        tx_root.clone(),
                    ));
                }
            }

            let col = view()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .h_full()
                .rounded(move || active_theme().radius)
                .bg(move || {
                    if drag_sig.get().is_some_and(|d| {
                        d.active && d.hover_status == Some(status) && d.from_status != status
                    }) {
                        link_color().opacity(0.08)
                    } else {
                        active_theme().secondary.opacity(0.35)
                    }
                })
                .border_1()
                .border_color(move || {
                    if drag_sig.get().is_some_and(|d| {
                        d.active && d.hover_status == Some(status) && d.from_status != status
                    }) {
                        link_color()
                    } else {
                        active_theme().border
                    }
                })
                .on_mouse_move(move |ev: &nsh_core::MouseEvent| {
                    if let Some(mut d) = drag_sig.get_untracked() {
                        if ev.pressed_button != Some(nsh_core::MouseButton::Left) {
                            drag_sig.set(None);
                            return;
                        }
                        let dx = ev.page_position.x.0 - d.start_x;
                        let dy = ev.page_position.y.0 - d.start_y;
                        if !d.active && (dx * dx + dy * dy).sqrt() >= DRAG_THRESHOLD_PX {
                            d.active = true;
                        }
                        if d.active && d.hover_status != Some(status) {
                            d.hover_status = Some(status);
                            drag_sig.set(Some(d));
                        } else if d.active
                            && !drag_sig.get_untracked().is_some_and(|prev| prev.active)
                        {
                            drag_sig.set(Some(d));
                        }
                    }
                })
                .on_mouse_leave(move |_: &nsh_core::MouseEvent| {
                    if let Some(mut d) = drag_sig.get_untracked()
                        && d.active
                        && d.hover_status == Some(status)
                    {
                        d.hover_status = None;
                        drag_sig.set(Some(d));
                    }
                })
                .on_mouse_up(move |_: &nsh_core::MouseEvent| {
                    if let Some(d) = drag_sig.get_untracked() {
                        drag_sig.set(None);
                        if d.active {
                            suppress_click_todo.replace(Some(d.todo_id));
                            if status != d.from_status {
                                send(
                                    &tx_col_drop,
                                    TodoBoardAction::SetStatus {
                                        id: d.todo_id,
                                        status,
                                    },
                                );
                            }
                        }
                    }
                })
                .child(col_hdr)
                .child(scroll().flex_1().min_h_0().w_full().child(list_col))
                .child(
                    view().flex_shrink_0().px_2().pb_2().child(
                        kumu_button("＋ 新建任务", ButtonVariant::Ghost)
                            .w_full()
                            .on_press(move || {
                                send(&tx_col_add_bottom, TodoBoardAction::OpenCreateModal(status));
                            }),
                    ),
                );

            grid = grid.child(col);
        }

        view()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(move || active_theme().background)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .on_mouse_move(move |ev: &nsh_core::MouseEvent| {
                if drag_sig.get_untracked().is_some()
                    && ev.pressed_button != Some(nsh_core::MouseButton::Left)
                {
                    drag_sig.set(None);
                }
            })
            .on_mouse_up(move |_: &nsh_core::MouseEvent| {
                if let Some(d) = drag_sig.get_untracked() {
                    if d.active {
                        suppress_click_todo.replace(Some(d.todo_id));
                    }
                    drag_sig.set(None);
                }
            })
            .on_mouse_leave(move |_: &nsh_core::MouseEvent| {
                if drag_sig.get_untracked().is_some() {
                    drag_sig.set(None);
                }
            })
            .child(header)
            .child(grid)
            .into_element()
    })
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc::unbounded;
    use nsh_engine::EngineConfig;

    use super::*;
    use crate::nsh_ui::mount::MountRegistry;

    struct BoardMount {
        initial: TodoBoardSnapshot,
        actions_tx: UnboundedSender<TodoBoardAction>,
    }

    static TEST_MOUNTS: MountRegistry<BoardMount> = MountRegistry::new();

    #[derive(Clone, Debug, PartialEq, Props)]
    struct TestBoardProps {
        mount_id: Prop<u64>,
    }

    #[component]
    fn TestBoardRoot(props: TestBoardProps) -> impl IntoElement {
        let mount = TEST_MOUNTS.take(props.mount_id.get_untracked());
        let (initial, tx) = match mount {
            Some(m) => (Some(m.initial), Some(m.actions_tx)),
            None => (None, None),
        };
        let sig = signal(initial);
        view().size_full().child(render_todo_board(sig, tx))
    }

    #[test]
    fn kanban_board_renders_three_columns_overflow_avatar_stack_and_actions() {
        let links: Vec<TodoLinkSnapshot> = (1..=5)
            .map(|i| TodoLinkSnapshot {
                id: i,
                agent_id: Some(if i % 2 == 0 { "codex" } else { "claude" }.into()),
                agent_name: if i % 2 == 0 { "Codex" } else { "Claude" }.into(),
                logo: None,
                session_id: Some(format!("ses_{i}")),
                pane_id: (i <= 2).then_some(i as u64),
                project_id: Some(1),
                worktree_name: Some("main".into()),
                label: format!("Session {i}"),
                state: (i == 1).then_some(AgentState::Waiting),
                awaiting_review: i == 1,
                can_resume: i > 2,
            })
            .collect();

        let snap = TodoBoardSnapshot {
            board_id: 1,
            board_name: "kumu".into(),
            launch_project_id: 1,
            todos: vec![TodoCardSnapshot {
                id: 12,
                project_id: 1,
                title: "Multi-agent Kanban card".into(),
                description: String::new(),
                status: TodoStatus::InProgress,
                awaiting_review: true,
                needs_attention: true,
                links,
                created_at: "2026-11-27".into(),
                updated_at: "2026-11-27".into(),
                completed_at: None,
            }],
            live_panes: Vec::new(),
            installed_agents: vec![("claude".into(), "Claude".into())],
            default_agent: Some("claude".into()),
            search_focus_nonce: 0,
        };

        let (actions_tx, mut actions_rx) = unbounded();
        let mount_id = TEST_MOUNTS.register(BoardMount {
            initial: snap,
            actions_tx,
        });
        let root = nsh_core::element::Root::new::<TestBoardRoot>(move || TestBoardProps {
            mount_id: mount_id.into(),
        });
        let mut app = nsh_testing::Headless::start(EngineConfig::new(root)).expect("start board");
        app.set_viewport(nsh_core::Size::new(
            nsh_core::Points(1100.0),
            nsh_core::Points(720.0),
        ))
        .expect("viewport");

        app.wait_for("待办").expect("todo column");
        app.wait_for("进行中").expect("in_progress column");
        app.wait_for("已完成").expect("done column");
        app.wait_for("需关注").expect("attention filter chip");
        app.wait_for("Multi-agent Kanban card").expect("card title");
        app.wait_for("+2").expect("5 links show 3 icons + +2");

        app.press("label:Multi-agent Kanban card")
            .expect("clicking card opens detail modal");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(TodoBoardAction::OpenDetailModal(12))
        );

        app.press("返回终端").expect("click back to terminal");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(TodoBoardAction::BackToTerminal)
        );
    }
}
