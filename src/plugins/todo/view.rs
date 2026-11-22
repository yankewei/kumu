//! Pure Rust NSH UI for the right-hand TODO Kanban board and inline detail editor.
//!
//! Zero GPUI imports: state flows in from the host via [`TodoHostEvent`] and
//! user intents flow back via [`TodoAction`].

use std::collections::HashMap;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use nsh_base::{input, theme as active_theme};
use nsh_core::element::desktop::{MenuItem, popup_menu, popup_menu_at};
use nsh_core::element::{AccessibilityRole, SignalEquals, scroll, signal_with};
use nsh_core::prelude::*;
use nsh_core::{ContextMenuEvent, MouseButton, MouseEvent};

use super::{
    TodoAction, TodoBoardSnapshot, TodoDispatchTarget, TodoHostEvent, short_agent_name, take_mount,
};
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoStatus};
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::{ButtonVariant, kumu_button, meta_text};
use crate::nsh_ui::theme::{TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_xs_size};
use crate::plugins::AgentPaneInfo;

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

fn send_action(tx: &Option<UnboundedSender<TodoAction>>, action: TodoAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

fn link_color() -> AdaptiveColor {
    kumu_color(TOKEN_LINK, |t| t.primary)
}

fn warning_color() -> AdaptiveColor {
    kumu_color(TOKEN_WARNING, |t| t.destructive)
}

fn success_color() -> AdaptiveColor {
    kumu_color(TOKEN_SUCCESS, |t| t.primary)
}

/// Build the shared dispatch/bind menu items for a TODO card or detail view.
fn build_dispatch_menu_items(
    tx: Option<UnboundedSender<TodoAction>>,
    item: TodoItem,
    installed_agents: Vec<(String, String)>,
    live_panes: Vec<AgentPaneInfo>,
    default_agent: Option<String>,
) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if installed_agents.is_empty() {
        items.push(MenuItem::new("未检测到已安装的 Agent").disabled(true));
    } else {
        for (agent_id, agent_name) in &installed_agents {
            let is_default = default_agent.as_deref() == Some(agent_id.as_str());
            let label = if is_default {
                format!("分屏启动 {agent_name}（默认）")
            } else {
                format!("分屏启动 {agent_name}")
            };
            let tx = tx.clone();
            let todo_id = item.id;
            let agent_id = agent_id.clone();
            items.push(MenuItem::new(label).on_select(move || {
                send_action(
                    &tx,
                    TodoAction::DispatchNewAgent {
                        id: todo_id,
                        agent_id: agent_id.clone(),
                        target: TodoDispatchTarget::SplitRight,
                    },
                );
            }));
        }
        let sub_items: Vec<MenuItem> = installed_agents
            .iter()
            .map(|(agent_id, agent_name)| {
                let tx = tx.clone();
                let todo_id = item.id;
                let agent_id = agent_id.clone();
                MenuItem::new(agent_name.clone()).on_select(move || {
                    send_action(
                        &tx,
                        TodoAction::DispatchNewAgent {
                            id: todo_id,
                            agent_id: agent_id.clone(),
                            target: TodoDispatchTarget::NewTab,
                        },
                    );
                })
            })
            .collect();
        items.push(MenuItem::submenu("在新页签中启动", sub_items));

        let wt_items: Vec<MenuItem> = installed_agents
            .iter()
            .map(|(agent_id, agent_name)| {
                let tx = tx.clone();
                let todo_id = item.id;
                let agent_id = agent_id.clone();
                MenuItem::new(agent_name.clone()).on_select(move || {
                    send_action(
                        &tx,
                        TodoAction::DispatchNewAgent {
                            id: todo_id,
                            agent_id: agent_id.clone(),
                            target: TodoDispatchTarget::NewWorktree,
                        },
                    );
                })
            })
            .collect();
        items.push(MenuItem::submenu("在新建 Worktree 中启动", wt_items));
    }

    if !live_panes.is_empty() {
        items.push(MenuItem::separator());
        for pane in &live_panes {
            let status = pane.state.map_or("空闲", AgentState::label);
            let label = format!(
                "发送到 窗格 {} · {}（{}）",
                pane.ordinal,
                short_agent_name(&pane.agent),
                status
            );
            let tx = tx.clone();
            let todo_id = item.id;
            let pane_info = pane.clone();
            items.push(MenuItem::new(label).on_select(move || {
                send_action(
                    &tx,
                    TodoAction::BindExistingPane {
                        id: todo_id,
                        pane: pane_info.clone(),
                        send_prompt: true,
                    },
                );
            }));
        }
        let link_items: Vec<MenuItem> = live_panes
            .iter()
            .map(|pane| {
                let status = pane.state.map_or("空闲", AgentState::label);
                let label = format!(
                    "窗格 {} · {}（{}）",
                    pane.ordinal,
                    short_agent_name(&pane.agent),
                    status
                );
                let tx = tx.clone();
                let todo_id = item.id;
                let pane_info = pane.clone();
                MenuItem::new(label).on_select(move || {
                    send_action(
                        &tx,
                        TodoAction::BindExistingPane {
                            id: todo_id,
                            pane: pane_info.clone(),
                            send_prompt: false,
                        },
                    );
                })
            })
            .collect();
        items.push(MenuItem::separator());
        items.push(MenuItem::submenu("仅关联已有窗格（不发送）", link_items));
    }
    items
}

#[allow(clippy::too_many_arguments)]
fn build_card_context_menu_items(
    tx: Option<UnboundedSender<TodoAction>>,
    open_detail: impl Fn(TodoItem) + Clone + 'static,
    item: TodoItem,
    jump_pane: Option<u64>,
    installed_agents: Vec<(String, String)>,
    live_panes: Vec<AgentPaneInfo>,
    default_agent: Option<String>,
) -> Vec<MenuItem> {
    let mut items = Vec::new();
    let id = item.id;
    if let Some(pane) = jump_pane {
        let tx = tx.clone();
        items.push(MenuItem::new("跳转到 Agent 窗格").on_select(move || {
            send_action(&tx, TodoAction::RevealPane(pane));
        }));
    }
    {
        let detail_item = item.clone();
        let open_detail = open_detail.clone();
        items.push(MenuItem::new("查看与编辑").on_select(move || {
            open_detail(detail_item.clone());
        }));
    }
    if item.status != TodoStatus::Done {
        let sub = build_dispatch_menu_items(
            tx.clone(),
            item.clone(),
            installed_agents,
            live_panes,
            default_agent,
        );
        items.push(MenuItem::submenu("派发 / 关联 Agent", sub));
    }
    items.push(MenuItem::separator());
    for status in [TodoStatus::Todo, TodoStatus::InProgress, TodoStatus::Done] {
        if item.status == status {
            continue;
        }
        let tx = tx.clone();
        let label = match status {
            TodoStatus::Todo => "移至待办",
            TodoStatus::InProgress => "移至进行中",
            TodoStatus::Done => "标记为已完成",
        };
        items.push(MenuItem::new(label).on_select(move || {
            send_action(&tx, TodoAction::SetStatus { id, status });
        }));
    }
    if item.pane_id.is_some() {
        let tx = tx.clone();
        items.push(MenuItem::new("解除 Agent 关联").on_select(move || {
            send_action(&tx, TodoAction::UnbindAgent(id));
        }));
    }
    items.push(MenuItem::separator());
    let tx_del = tx.clone();
    items.push(
        MenuItem::new("删除任务")
            .destructive(true)
            .on_select(move || {
                send_action(&tx_del, TodoAction::DeleteItem(id));
            }),
    );
    items
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

    let quick_draft = signal(String::new());
    let composer_open = signal(false);
    let composer_title = signal(String::new());
    let composer_desc = signal(String::new());

    let in_progress_open = signal(true);
    let todo_open = signal(true);
    let done_open = signal(false);

    // Inline detail editor state (`← 返回看板`).
    let detail_id = signal(Option::<i64>::None);
    let detail_title = signal(String::new());
    let detail_desc = signal(String::new());
    let detail_followup = signal(String::new());

    // Lightweight mouse drag state across lanes (never interferes with wheel scroll).
    let drag_candidate = signal(Option::<DragCandidate>::None);
    let active_drag = signal(Option::<ActiveDrag>::None);
    let drag_over_lane = signal(Option::<TodoStatus>::None);

    let open_detail = move |item: TodoItem| {
        notice.set(None);
        detail_title.set(item.title);
        detail_desc.set(item.description);
        detail_followup.set(String::new());
        detail_id.set(Some(item.id));
    };

    let close_detail = move || {
        detail_id.set(None);
        notice.set(None);
    };

    if let Some(mut rx) = events_rx {
        spawn(async move {
            while let Some(event) = rx.next().await {
                match event {
                    TodoHostEvent::Snapshot(next) => {
                        let prev_board = board.get_untracked().board_id;
                        let next_board = next.board_id;
                        if prev_board != next_board {
                            detail_id.set(None);
                            notice.set(None);
                        } else if let Some(id) = detail_id.get_untracked()
                            && !next.todos.iter().any(|t| t.id == id)
                        {
                            detail_id.set(None);
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
                        in_progress_open.set(true);
                    }
                    TodoHostEvent::ClearFollowup => {
                        detail_followup.set(String::new());
                    }
                }
            }
        });
    }

    let submit_quick = {
        let tx = actions_tx.clone();
        move |dispatch: bool| {
            let title = quick_draft.get_untracked().trim().to_string();
            if title.is_empty() {
                return;
            }
            quick_draft.set(String::new());
            notice.set(None);
            send_action(
                &tx,
                TodoAction::CreateTodo {
                    title,
                    description: String::new(),
                    dispatch_default: dispatch,
                },
            );
        }
    };

    let submit_composer = {
        let tx = actions_tx.clone();
        move |dispatch: bool| {
            let title = composer_title.get_untracked().trim().to_string();
            if title.is_empty() {
                return;
            }
            let description = composer_desc.get_untracked().trim().to_string();
            composer_title.set(String::new());
            composer_desc.set(String::new());
            composer_open.set(false);
            notice.set(None);
            send_action(
                &tx,
                TodoAction::CreateTodo {
                    title,
                    description,
                    dispatch_default: dispatch,
                },
            );
        }
    };

    let render_notice_banner = move || {
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
    };

    // ---- Detail view renderer (`← 返回看板`) ----
    let tx_detail_root = actions_tx.clone();
    let render_detail_view = move |item: TodoItem, snap: TodoBoardSnapshot| {
        let tx = tx_detail_root.clone();
        let pane_map: HashMap<u64, AgentPaneInfo> = snap
            .live_panes
            .iter()
            .cloned()
            .map(|p| (p.pane, p))
            .collect();
        let live_pane = item.pane_id.and_then(|p| pane_map.get(&p)).cloned();
        let live_state = live_pane.as_ref().and_then(|p| p.state);
        let needs_review = item.awaiting_review || live_state == Some(AgentState::Completed);
        let id = item.id;

        let tx_del = tx.clone();
        let header = view()
            .flex_row()
            .flex_shrink_0()
            .h(px(32.0))
            .px_2()
            .gap_1p5()
            .items_center()
            .border_b_1()
            .border_b_color(move || active_theme().sidebar_border)
            .child(
                view()
                    .role(AccessibilityRole::Button)
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .py_0p5()
                    .rounded(move || active_theme().radius)
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                    .on_press(close_detail)
                    .child(
                        icon_svg("icons/arrow-left.svg", 13.0)
                            .text_color(move || active_theme().foreground),
                    )
                    .child(
                        text("返回看板")
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground),
                    ),
            )
            .child(view().flex_1())
            .child(
                text(format!("#{id}"))
                    .font_family(move || {
                        active_theme()
                            .font_mono
                            .clone()
                            .unwrap_or_else(|| SharedString::from(crate::fonts::TERM_FONT))
                    })
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().muted_foreground),
            )
            .child(
                view()
                    .role(AccessibilityRole::Button)
                    .label("删除任务")
                    .p_1()
                    .rounded(move || active_theme().radius)
                    .hover(|s| s.bg(move || active_theme().destructive.opacity(0.14)))
                    .on_press(move || {
                        close_detail();
                        send_action(&tx_del, TodoAction::DeleteItem(id));
                    })
                    .child(
                        icon_svg("icons/trash.svg", 13.0)
                            .text_color(move || active_theme().muted_foreground),
                    ),
            );

        // Status segmented control
        let mut status_seg = view()
            .flex_row()
            .w_full()
            .p_0p5()
            .gap_0p5()
            .rounded(move || active_theme().radius)
            .bg(move || active_theme().background)
            .border_1()
            .border_color(move || active_theme().border);

        for status in [TodoStatus::Todo, TodoStatus::InProgress, TodoStatus::Done] {
            let active = item.status == status;
            let tx_st = tx.clone();
            let mut seg_btn = view()
                .role(AccessibilityRole::Button)
                .flex_1()
                .h(px(24.0))
                .rounded(px(4.0))
                .items_center()
                .justify_center()
                .on_press(move || send_action(&tx_st, TodoAction::SetStatus { id, status }));
            if active {
                seg_btn = seg_btn.bg(move || active_theme().sidebar_accent);
            } else {
                seg_btn =
                    seg_btn.hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.4)));
            }
            seg_btn = seg_btn.child(
                text(status.label())
                    .text_size(text_xs_size)
                    .font_weight(if active {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(move || {
                        if active {
                            active_theme().sidebar_accent_foreground
                        } else {
                            active_theme().muted_foreground
                        }
                    }),
            );
            status_seg = status_seg.child(seg_btn);
        }

        // Agent section
        let agent_section: AnyElement = if let Some(pane) = live_pane {
            let (pill_color_kind, pill_label) = match live_state {
                Some(AgentState::Waiting) => {
                    (0u8, format!("{} · 等待输入", short_agent_name(&pane.agent)))
                }
                Some(AgentState::Completed) => {
                    (1u8, format!("{} · 待确认", short_agent_name(&pane.agent)))
                }
                _ if needs_review => (1u8, format!("{} · 待确认", short_agent_name(&pane.agent))),
                Some(AgentState::Failed) => {
                    (2u8, format!("{} · 执行失败", short_agent_name(&pane.agent)))
                }
                Some(AgentState::Running) => {
                    (3u8, format!("{} · 运行中", short_agent_name(&pane.agent)))
                }
                None => (4u8, format!("{} · 空闲", short_agent_name(&pane.agent))),
            };
            let pill_fg = move || match pill_color_kind {
                0 => warning_color(),
                1 => success_color(),
                2 => active_theme().destructive,
                3 => link_color(),
                _ => active_theme().muted_foreground,
            };
            let pill_bg = move || match pill_color_kind {
                0 => warning_color().opacity(0.16),
                1 => success_color().opacity(0.16),
                2 => active_theme().destructive.opacity(0.16),
                3 => link_color().opacity(0.14),
                _ => active_theme().sidebar_accent.opacity(0.7),
            };

            let pane_id = pane.pane;
            let tx_jump = tx.clone();
            let tx_unbind = tx.clone();
            let tx_followup_submit = tx.clone();
            let tx_followup_btn = tx.clone();

            view()
                .flex_col()
                .w_full()
                .gap_2()
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .p_2p5()
                        .gap_2()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().background)
                        .border_1()
                        .border_color(move || active_theme().border)
                        .child(
                            view()
                                .flex_row()
                                .w_full()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    view()
                                        .flex_row()
                                        .h(px(20.0))
                                        .px_1p5()
                                        .gap_1()
                                        .rounded(px(5.0))
                                        .bg(pill_bg)
                                        .items_center()
                                        .child(
                                            view()
                                                .w(px(6.0))
                                                .h(px(6.0))
                                                .rounded(px(3.0))
                                                .bg(pill_fg),
                                        )
                                        .child(
                                            text(pill_label)
                                                .text_size(text_xs_size)
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(pill_fg),
                                        ),
                                )
                                .child(view().flex_1())
                                .child(meta_text(format!("窗格 {}", pane.ordinal))),
                        )
                        .child(
                            view()
                                .flex_row()
                                .w_full()
                                .gap_1p5()
                                .child(
                                    kumu_button("跳转窗格", ButtonVariant::Ghost)
                                        .flex_1()
                                        .on_press(move || {
                                            send_action(&tx_jump, TodoAction::RevealPane(pane_id));
                                        }),
                                )
                                .child(kumu_button("解绑", ButtonVariant::Ghost).on_press(
                                    move || {
                                        send_action(&tx_unbind, TodoAction::UnbindAgent(id));
                                    },
                                )),
                        ),
                )
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .gap_1()
                        .child(meta_text("快速追问（直接发给该 Agent）"))
                        .child(
                            view()
                                .flex_row()
                                .w_full()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    input()
                                        .flex_1()
                                        .px_2()
                                        .py_1()
                                        .rounded(move || active_theme().radius)
                                        .border_1()
                                        .border_color(move || active_theme().input)
                                        .bg(move || active_theme().background)
                                        .placeholder("补充修改意见，回车发送…")
                                        .placeholder_color(move || {
                                            active_theme().muted_foreground.light
                                        })
                                        .text_size(text_xs_size)
                                        .text_color(move || active_theme().foreground)
                                        .value(move || detail_followup.get())
                                        .on_change(move |v: &str| detail_followup.set(v.to_owned()))
                                        .on_submit(move |_: &str| {
                                            let msg =
                                                detail_followup.get_untracked().trim().to_string();
                                            if !msg.is_empty() {
                                                send_action(
                                                    &tx_followup_submit,
                                                    TodoAction::SendFollowup { id, text: msg },
                                                );
                                            }
                                        }),
                                )
                                .child(
                                    view()
                                        .role(AccessibilityRole::Button)
                                        .label("发送追问")
                                        .p_1p5()
                                        .rounded(move || active_theme().radius)
                                        .bg(move || active_theme().background)
                                        .border_1()
                                        .border_color(move || active_theme().border)
                                        .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                        .on_press(move || {
                                            let msg =
                                                detail_followup.get_untracked().trim().to_string();
                                            if !msg.is_empty() {
                                                send_action(
                                                    &tx_followup_btn,
                                                    TodoAction::SendFollowup { id, text: msg },
                                                );
                                            }
                                        })
                                        .child(
                                            icon_svg("icons/arrow-right.svg", 13.0)
                                                .text_color(move || active_theme().foreground),
                                        ),
                                ),
                        ),
                )
                .into_element()
        } else {
            let default_label = snap
                .default_agent
                .as_deref()
                .map(short_agent_name)
                .unwrap_or_else(|| "Agent".to_string());
            let tx_default = tx.clone();
            let tx_menu = tx.clone();
            let menu_item = item.clone();
            let menu_installed = snap.installed_agents.clone();
            let menu_panes = snap.live_panes.clone();
            let menu_default = snap.default_agent.clone();

            let mut box_el = view()
                .flex_col()
                .w_full()
                .p_2p5()
                .gap_2()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().background)
                .border_1()
                .border_color(move || active_theme().border);
            if let Some(prev) = item.agent_id.as_deref() {
                box_el = box_el.child(meta_text(format!(
                    "上次关联：{}（窗格已关闭）",
                    short_agent_name(prev)
                )));
            }
            box_el
                .child(
                    view()
                        .flex_row()
                        .w_full()
                        .gap_1p5()
                        .child(
                            kumu_button(format!("交给 {default_label}"), ButtonVariant::Primary)
                                .flex_1()
                                .on_press(move || {
                                    send_action(&tx_default, TodoAction::DispatchDefault(id));
                                }),
                        )
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("更多派发或关联方式")
                                .px_2()
                                .py_1()
                                .rounded(move || active_theme().radius)
                                .border_1()
                                .border_color(move || active_theme().border)
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                .items_center()
                                .justify_center()
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
                                    icon_svg("icons/chevron-down.svg", 13.0)
                                        .text_color(move || active_theme().muted_foreground),
                                ),
                        ),
                )
                .into_element()
        };

        let tx_footer = tx.clone();
        let footer_cta: AnyElement = if item.status == TodoStatus::Done {
            kumu_button("重新打开（移回待办）", ButtonVariant::Ghost)
                .w_full()
                .on_press(move || {
                    send_action(
                        &tx_footer,
                        TodoAction::SetStatus {
                            id,
                            status: TodoStatus::Todo,
                        },
                    );
                })
                .into_element()
        } else if needs_review {
            view()
                .role(AccessibilityRole::Button)
                .w_full()
                .py_1p5()
                .px_3()
                .rounded(move || active_theme().radius)
                .bg(move || success_color().opacity(0.16))
                .hover(|s| s.bg(move || success_color().opacity(0.26)))
                .flex_row()
                .items_center()
                .justify_center()
                .gap_1p5()
                .on_press(move || {
                    send_action(
                        &tx_footer,
                        TodoAction::SetStatus {
                            id,
                            status: TodoStatus::Done,
                        },
                    );
                    close_detail();
                })
                .child(icon_svg("icons/check.svg", 13.0).text_color(success_color))
                .child(
                    text("确认完成并归档")
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(success_color),
                )
                .into_element()
        } else {
            kumu_button("标记为已完成", ButtonVariant::Ghost)
                .w_full()
                .on_press(move || {
                    send_action(
                        &tx_footer,
                        TodoAction::SetStatus {
                            id,
                            status: TodoStatus::Done,
                        },
                    );
                    close_detail();
                })
                .into_element()
        };

        let tx_title = tx.clone();
        let tx_desc = tx.clone();
        let body = scroll().flex_1().child(
            view()
                .flex_col()
                .w_full()
                .p_3()
                .gap_3()
                .child(render_notice_banner())
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .gap_1()
                        .child(meta_text("标题"))
                        .child(
                            input()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .rounded(move || active_theme().radius)
                                .border_1()
                                .border_color(move || active_theme().input)
                                .bg(move || active_theme().background)
                                .placeholder("任务标题")
                                .placeholder_color(move || active_theme().muted_foreground.light)
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().foreground)
                                .value(move || detail_title.get())
                                .on_change(move |v: &str| {
                                    detail_title.set(v.to_owned());
                                    let trimmed = v.trim().to_string();
                                    if !trimmed.is_empty() {
                                        send_action(
                                            &tx_title,
                                            TodoAction::UpdateTitle { id, title: trimmed },
                                        );
                                    }
                                }),
                        ),
                )
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .gap_1()
                        .child(meta_text("状态"))
                        .child(status_seg),
                )
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .gap_1()
                        .child(meta_text("任务说明 / Agent Prompt"))
                        .child(
                            input()
                                .w_full()
                                .min_h(px(84.0))
                                .max_h(px(180.0))
                                .multiline(true)
                                .px_2()
                                .py_1p5()
                                .rounded(move || active_theme().radius)
                                .border_1()
                                .border_color(move || active_theme().border)
                                .bg(move || active_theme().background)
                                .placeholder("任务说明（派发时作为 Prompt 发送给 Agent）…")
                                .placeholder_color(move || active_theme().muted_foreground.light)
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().foreground)
                                .value(move || detail_desc.get())
                                .on_change(move |v: &str| {
                                    detail_desc.set(v.to_owned());
                                    send_action(
                                        &tx_desc,
                                        TodoAction::UpdateDescription {
                                            id,
                                            description: v.to_owned(),
                                        },
                                    );
                                }),
                        ),
                )
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .gap_1()
                        .child(meta_text("关联 Agent"))
                        .child(agent_section),
                ),
        );

        view()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .bg(move || active_theme().sidebar)
            .child(header)
            .child(body)
            .child(
                view()
                    .flex_shrink_0()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_t_color(move || active_theme().sidebar_border)
                    .child(footer_cta),
            )
    };

    // ---- Main board view ----
    let tx_board_root = actions_tx.clone();
    let tx_drop = actions_tx.clone();

    let content = dynamic(move || {
        let snap = board.get();
        if !snap.has_project {
            return view()
                .flex_1()
                .px_3()
                .py_4()
                .child(meta_text("没有选中的项目"))
                .into_element();
        }

        if let Some(id) = detail_id.get()
            && let Some(item) = snap.todos.iter().find(|t| t.id == id).cloned()
        {
            return render_detail_view(item, snap).into_element();
        }

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

        // Quick-add / expanded composer section
        let mut quick_wrap = view()
            .flex_col()
            .flex_shrink_0()
            .px_2p5()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_b_color(move || active_theme().sidebar_border);

        if let Some(shared_from) = snap.shared_board_name.as_ref()
            && !shared_from.is_empty()
        {
            quick_wrap = quick_wrap.child(meta_text(format!("共享自 {shared_from}")));
        }

        if composer_open.get() {
            let submit_add = submit_composer.clone();
            let submit_run = submit_composer.clone();
            let submit_enter = submit_composer.clone();
            quick_wrap = quick_wrap.child(
                view()
                    .flex_col()
                    .p_2()
                    .gap_1p5()
                    .rounded(move || active_theme().radius)
                    .bg(move || active_theme().background)
                    .border_1()
                    .border_color(move || active_theme().border)
                    .child(
                        input()
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded(move || active_theme().radius)
                            .border_1()
                            .border_color(move || active_theme().input)
                            .bg(move || active_theme().background)
                            .placeholder("任务标题")
                            .placeholder_color(move || active_theme().muted_foreground.light)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground)
                            .value(move || composer_title.get())
                            .on_change(move |v: &str| composer_title.set(v.to_owned()))
                            .on_submit(move |_: &str| submit_enter(false)),
                    )
                    .child(
                        input()
                            .w_full()
                            .min_h(px(56.0))
                            .max_h(px(120.0))
                            .multiline(true)
                            .px_2()
                            .py_1()
                            .rounded(move || active_theme().radius)
                            .bg(move || active_theme().sidebar.opacity(0.65))
                            .placeholder("详细说明 / Agent Prompt（可选）…")
                            .placeholder_color(move || active_theme().muted_foreground.light)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground)
                            .value(move || composer_desc.get())
                            .on_change(move |v: &str| composer_desc.set(v.to_owned())),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_1p5()
                            .items_center()
                            .child(
                                kumu_button("仅添加", ButtonVariant::Ghost)
                                    .on_press(move || submit_add(false)),
                            )
                            .child(
                                kumu_button(
                                    format!("添加并交给 {default_label}"),
                                    ButtonVariant::Primary,
                                )
                                .on_press(move || submit_run(true)),
                            )
                            .child(view().flex_1())
                            .child(
                                view()
                                    .role(AccessibilityRole::Button)
                                    .label("收起")
                                    .p_1()
                                    .rounded(move || active_theme().radius)
                                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                    .on_press(move || composer_open.set(false))
                                    .child(
                                        icon_svg("icons/x.svg", 12.0)
                                            .text_color(move || active_theme().muted_foreground),
                                    ),
                            ),
                    ),
            );
        } else {
            let submit_quick_enter = submit_quick.clone();
            quick_wrap = quick_wrap.child(
                view()
                    .flex_row()
                    .gap_1p5()
                    .items_center()
                    .child(
                        input()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded(move || active_theme().radius)
                            .border_1()
                            .border_color(move || active_theme().input)
                            .bg(move || active_theme().background)
                            .placeholder("添加待办…")
                            .placeholder_color(move || active_theme().muted_foreground.light)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground)
                            .value(move || quick_draft.get())
                            .on_change(move |v: &str| quick_draft.set(v.to_owned()))
                            .on_submit(move |_: &str| submit_quick_enter(false)),
                    )
                    .child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("展开多行说明")
                            .p_1p5()
                            .rounded(move || active_theme().radius)
                            .bg(move || active_theme().background)
                            .border_1()
                            .border_color(move || active_theme().border)
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                            .on_press(move || {
                                let draft = quick_draft.get_untracked();
                                if !draft.trim().is_empty() {
                                    composer_title.set(draft);
                                    quick_draft.set(String::new());
                                }
                                composer_open.set(true);
                            })
                            .child(
                                icon_svg("icons/plus.svg", 13.0)
                                    .text_color(move || active_theme().foreground),
                            ),
                    ),
            );
        }

        quick_wrap = quick_wrap.child(render_notice_banner());

        // Helper for lane headers
        let tx_clear = tx_board_root.clone();
        let make_lane_header = move |status: TodoStatus, count: usize, open_sig: Signal<bool>| {
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
                let tx_clear = tx_clear.clone();
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
        };

        // Helper for wrapping a lane with drag-over highlight
        let make_lane = move |target: TodoStatus,
                              header: View,
                              cards: Vec<AnyElement>,
                              expanded: bool,
                              empty_hint: &'static str| {
            let mut lane = view()
                .flex_col()
                .w_full()
                .gap_1p5()
                .p_0p5()
                .rounded(move || active_theme().radius)
                .style_when(
                    move || active_drag.get().is_some() && drag_over_lane.get() == Some(target),
                    |s| {
                        s.bg(move || link_color().opacity(0.08))
                            .border_1()
                            .border_color(move || link_color().opacity(0.45))
                    },
                )
                .on_mouse_enter(move |_: &MouseEvent| {
                    if active_drag.get_untracked().is_some() {
                        drag_over_lane.set(Some(target));
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
        };

        // Render In-Progress cards
        let in_progress_cards: Vec<AnyElement> = in_progress
            .iter()
            .map(|item| {
                let live_pane = item.pane_id.and_then(|p| pane_map.get(&p));
                let live_state = live_pane.and_then(|p| p.state);
                let needs_review =
                    item.awaiting_review || live_state == Some(AgentState::Completed);
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
                    let tx = tx_board_root.clone();
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
                    let tx = tx_board_root.clone();
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
                    let tx = tx_board_root.clone();
                    let menu_item = item.clone();
                    let installed = snap.installed_agents.clone();
                    let panes = snap.live_panes.clone();
                    let def = snap.default_agent.clone();
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

                let more_item = item.clone();
                let card_item = item.clone();
                let tx_jump = tx_board_root.clone();
                let drag_title = item.title.clone();

                let ctx_tx = tx_board_root.clone();
                let ctx_item = item.clone();
                let ctx_installed = snap.installed_agents.clone();
                let ctx_panes = snap.live_panes.clone();
                let ctx_default = snap.default_agent.clone();

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
                    )
                    .child({
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
                        drag_candidate.set(Some(DragCandidate {
                            id,
                            title: drag_title.clone(),
                            start_y: None,
                        }));
                    })
                    .on_press(move || {
                        if active_drag.get_untracked().is_some() {
                            return;
                        }
                        if let Some(pane) = jump_pane {
                            send_action(&tx_jump, TodoAction::RevealPane(pane));
                        } else {
                            open_detail(card_item.clone());
                        }
                    })
                    .on_context_menu(move |ev: &ContextMenuEvent| {
                        let items = build_card_context_menu_items(
                            ctx_tx.clone(),
                            open_detail,
                            ctx_item.clone(),
                            jump_pane,
                            ctx_installed.clone(),
                            ctx_panes.clone(),
                            ctx_default.clone(),
                        );
                        drop(popup_menu_at(items, ev.page_position));
                    })
                    .child(body)
                    .into_element()
            })
            .collect();

        // Render Pending Todo cards
        let todo_cards: Vec<AnyElement> = todo_items
            .iter()
            .map(|item| {
                let id = item.id;
                let drag_title = item.title.clone();
                let open_item = item.clone();
                let more_item = item.clone();
                let tx_dispatch = tx_board_root.clone();
                let tx_menu = tx_board_root.clone();
                let tx_done = tx_board_root.clone();

                let menu_item = item.clone();
                let menu_installed = snap.installed_agents.clone();
                let menu_panes = snap.live_panes.clone();
                let menu_default = snap.default_agent.clone();

                let ctx_tx = tx_board_root.clone();
                let ctx_item = item.clone();
                let ctx_installed = snap.installed_agents.clone();
                let ctx_panes = snap.live_panes.clone();
                let ctx_default = snap.default_agent.clone();

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
                        icon_svg("icons/check.svg", 13.0)
                            .text_color(move || active_theme().muted_foreground),
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
                        drag_candidate.set(Some(DragCandidate {
                            id,
                            title: drag_title.clone(),
                            start_y: None,
                        }));
                    })
                    .on_press(move || {
                        if active_drag.get_untracked().is_some() {
                            return;
                        }
                        open_detail(open_item.clone());
                    })
                    .on_context_menu(move |ev: &ContextMenuEvent| {
                        let items = build_card_context_menu_items(
                            ctx_tx.clone(),
                            open_detail,
                            ctx_item.clone(),
                            None,
                            ctx_installed.clone(),
                            ctx_panes.clone(),
                            ctx_default.clone(),
                        );
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
            })
            .collect();

        // Render Done rows
        let done_rows: Vec<AnyElement> = done_items
            .iter()
            .map(|item| {
                let id = item.id;
                let drag_title = item.title.clone();
                let open_item = item.clone();
                let ctx_tx = tx_board_root.clone();
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
                        drag_candidate.set(Some(DragCandidate {
                            id,
                            title: drag_title.clone(),
                            start_y: None,
                        }));
                    })
                    .on_press(move || {
                        if active_drag.get_untracked().is_some() {
                            return;
                        }
                        open_detail(open_item.clone());
                    })
                    .on_context_menu(move |ev: &ContextMenuEvent| {
                        let items = build_card_context_menu_items(
                            ctx_tx.clone(),
                            open_detail,
                            ctx_item.clone(),
                            None,
                            Vec::new(),
                            Vec::new(),
                            None,
                        );
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
                if let Some(agent) = item.agent_id.as_deref() {
                    row = row.child(meta_text(short_agent_name(agent)));
                }
                row.into_element()
            })
            .collect();

        let ip_hdr = make_lane_header(TodoStatus::InProgress, in_progress.len(), in_progress_open);
        let todo_hdr = make_lane_header(TodoStatus::Todo, todo_items.len(), todo_open);
        let done_hdr = make_lane_header(TodoStatus::Done, done_items.len(), done_open);

        let body = scroll().flex_1().child(
            view()
                .flex_col()
                .w_full()
                .px_2()
                .py_2()
                .gap_2()
                .child(make_lane(
                    TodoStatus::InProgress,
                    ip_hdr,
                    in_progress_cards,
                    in_progress_open.get(),
                    "暂无进行中的任务",
                ))
                .child(
                    view()
                        .w_full()
                        .h(px(1.0))
                        .flex_shrink_0()
                        .bg(move || active_theme().sidebar_border),
                )
                .child(make_lane(
                    TodoStatus::Todo,
                    todo_hdr,
                    todo_cards,
                    todo_open.get(),
                    "在上方输入框添加待办",
                ))
                .child(
                    view()
                        .w_full()
                        .h(px(1.0))
                        .flex_shrink_0()
                        .bg(move || active_theme().sidebar_border),
                )
                .child(make_lane(
                    TodoStatus::Done,
                    done_hdr,
                    done_rows,
                    done_open.get(),
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
    });

    // Floating drag preview chip
    let drag_chip = dynamic(move || {
        let Some(drag) = active_drag.get() else {
            return view().into_element();
        };
        view()
            .absolute()
            .left(px(12.0))
            .top(px((drag.y - 16.0).max(8.0)))
            .max_w(px(220.0))
            .px_2p5()
            .py_1p5()
            .rounded(move || active_theme().radius)
            .bg(move || active_theme().background)
            .border_1()
            .border_color(link_color)
            .pointer_events(PointerEvents::None)
            .child(
                text(drag.title)
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
            if let Some(mut cur) = active_drag.get_untracked() {
                cur.y = ev.page_position.y.0;
                active_drag.set(Some(cur));
            } else if let Some(mut cand) = drag_candidate.get_untracked() {
                let y = ev.page_position.y.0;
                match cand.start_y {
                    None => {
                        cand.start_y = Some(y);
                        drag_candidate.set(Some(cand));
                    }
                    Some(start) if (y - start).abs() > 6.0 => {
                        active_drag.set(Some(ActiveDrag {
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
            if let Some(drag) = active_drag.get_untracked()
                && let Some(status) = drag_over_lane.get_untracked()
            {
                send_action(
                    &tx_drop,
                    TodoAction::SetStatus {
                        id: drag.id,
                        status,
                    },
                );
            }
            drag_candidate.set(None);
            active_drag.set(None);
            drag_over_lane.set(None);
        })
        .child(content)
        .child(drag_chip)
}
