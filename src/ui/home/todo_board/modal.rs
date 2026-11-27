//! Full-window Pure Rust NSH overlay for TODO Create & Jira-style Two-Column
//! Issue Detail modals.
//!
//! Mounted ONLY while a modal is open on top of `KumuApp` (same pattern as
//! `crate::nsh_ui::dialogs::ActiveDialog`), so the backdrop covers both the
//! sidebar and main area and window shortcuts are cleanly guarded.

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{AppContext, Context, Entity, Task, Window};
use nsh_base::theme as active_theme;
use nsh_core::element::desktop::{MenuItem, popup_menu};
use nsh_core::element::scroll;
use nsh_core::prelude::*;
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::{
    TodoBoardAction, TodoBoardSnapshot, TodoCardSnapshot, TodoDispatchTarget, TodoLinkSnapshot,
    short_agent_name,
};
use crate::agent::AgentState;
use crate::db::TodoStatus;
use crate::nsh_ui::assets::{agent_logo, icon_svg};
use crate::nsh_ui::components::{
    ButtonVariant, ControlTone, body_text, kumu_button, kumu_icon_button, kumu_input, kumu_tooltip,
    meta_text,
};
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{
    appearance, link_color, nsh_theme, success_color, text_sm_size, text_xs_size, warning_color,
};
use crate::theme::KumuTheme;

#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui::home) enum TodoModalState {
    Create {
        initial_status: TodoStatus,
        board: TodoBoardSnapshot,
    },
    Detail {
        todo_id: i64,
        board: TodoBoardSnapshot,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum TodoModalEvent {
    State(TodoModalState),
    RequestClose,
}

struct ModalMount {
    initial: TodoModalState,
    events_rx: UnboundedReceiver<TodoModalEvent>,
    actions_tx: UnboundedSender<TodoBoardAction>,
}

static MOUNTS: MountRegistry<ModalMount> = MountRegistry::new();

fn send(tx: &Option<UnboundedSender<TodoBoardAction>>, action: TodoBoardAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

#[derive(Clone, Debug, PartialEq, Props)]
struct TodoModalRootProps {
    mount_id: Prop<u64>,
}

fn render_status_seg(
    current: TodoStatus,
    on_select: impl Fn(TodoStatus) + Clone + 'static,
) -> AnyElement {
    let options = [
        (TodoStatus::Todo, "待办"),
        (TodoStatus::InProgress, "进行中"),
        (TodoStatus::Done, "已完成"),
    ];
    let mut row = view()
        .flex_row()
        .w_full()
        .p_0p5()
        .gap_0p5()
        .rounded(move || active_theme().radius)
        .bg(move || active_theme().muted);

    for (status, label) in options {
        let active = current == status;
        let cb = on_select.clone();
        row = row.child(
            nsh_base::button()
                .flex_1()
                .h(px(26.0))
                .items_center()
                .justify_center()
                .rounded(move || active_theme().radius)
                .bg(move || {
                    if active {
                        active_theme().card
                    } else {
                        AdaptiveColor::TRANSPARENT
                    }
                })
                .on_press(move || cb(status))
                .child(
                    text(label)
                        .text_size(text_xs_size)
                        .font_weight(if active {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(move || {
                            if active {
                                active_theme().foreground
                            } else {
                                active_theme().muted_foreground
                            }
                        }),
                ),
        );
    }
    row.into_element()
}

fn render_link_row(
    todo_id: i64,
    link: &TodoLinkSnapshot,
    followup_link_id: Signal<Option<i64>>,
    followup_text: Signal<String>,
    tx: Option<UnboundedSender<TodoBoardAction>>,
) -> AnyElement {
    let link_id = link.id;
    let is_live = link.pane_id.is_some();
    let pane_id = link.pane_id;
    let can_resume = link.can_resume;
    let state = link.state;
    let awaiting = link.awaiting_review;
    let needs_attn = is_live && (state == Some(AgentState::Waiting) || awaiting);

    let status_label = if is_live {
        if state == Some(AgentState::Waiting) {
            "等待输入"
        } else if awaiting {
            "待确认"
        } else if state == Some(AgentState::Failed) {
            "执行失败"
        } else if state == Some(AgentState::Running) {
            "运行中"
        } else {
            "在线"
        }
    } else {
        "历史会话"
    };

    let pill_color = move || {
        if !is_live {
            active_theme().muted_foreground
        } else if state == Some(AgentState::Waiting) || awaiting {
            warning_color()
        } else if state == Some(AgentState::Failed) {
            active_theme().destructive
        } else if state == Some(AgentState::Running) {
            link_color()
        } else {
            success_color()
        }
    };

    let icon_el: AnyElement = if link.agent_id.is_none() {
        icon_svg("icons/square-terminal.svg", 13.0)
            .text_color(move || active_theme().muted_foreground)
            .into_element()
    } else {
        agent_logo(link.logo, 13.0)
    };

    let mut left = view()
        .flex_row()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_1p5()
        .child(icon_el)
        .child(
            view()
                .px_1p5()
                .py_0p5()
                .rounded(px(4.0))
                .bg(move || pill_color().opacity(0.14))
                .child(
                    text(format!("{} · {status_label}", link.agent_name))
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(pill_color),
                ),
        );

    if !link.label.is_empty() {
        left = left.child(
            text(link.label.clone())
                .flex_1()
                .min_w_0()
                .app_class("truncate")
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().foreground),
        );
    }
    if let Some(ses) = &link.session_id {
        let short_ses: String = if ses.chars().count() > 12 {
            let mut s: String = ses.chars().take(12).collect();
            s.push('…');
            s
        } else {
            ses.clone()
        };
        left = left.child(
            view()
                .px_1p5()
                .py_0p5()
                .rounded(px(4.0))
                .bg(move || active_theme().muted)
                .tooltip(kumu_tooltip(format!("Session: {ses}")))
                .child(meta_text(short_ses)),
        );
    }
    if let Some(wt) = &link.worktree_name {
        left = left.child(meta_text(format!("⎇ {wt}")));
    }

    let mut actions = view().flex_row().flex_shrink_0().items_center().gap_1();
    if let Some(pid) = pane_id {
        let tx_jump = tx.clone();
        let tx_ack = tx.clone();
        let tx_detach = tx.clone();
        actions = actions
            .child(
                kumu_button("↗ 终端", ButtonVariant::Primary).on_press(move || {
                    send(&tx_jump, TodoBoardAction::RevealPane(pid));
                }),
            )
            .child(
                kumu_button("追问", ButtonVariant::Default).on_press(move || {
                    if followup_link_id.get_untracked() == Some(link_id) {
                        followup_link_id.set(None);
                    } else {
                        followup_text.set(String::new());
                        followup_link_id.set(Some(link_id));
                    }
                }),
            );
        if awaiting {
            actions = actions.child(kumu_button("已确认", ButtonVariant::Ghost).on_press(
                move || {
                    send(
                        &tx_ack,
                        TodoBoardAction::ClearReview {
                            todo_id,
                            link_id: Some(link_id),
                        },
                    );
                },
            ));
        }
        actions = actions.child(kumu_button("断开", ButtonVariant::Ghost).on_press(move || {
            send(&tx_detach, TodoBoardAction::DetachLinkPane(link_id));
        }));
    } else {
        let tx_resume = tx.clone();
        let tx_remove = tx.clone();
        let resume_btn = kumu_button("↺ 恢复 Session", ButtonVariant::Primary)
            .disabled(!can_resume)
            .on_press(move || {
                if can_resume {
                    send(&tx_resume, TodoBoardAction::ResumeLink(link_id));
                }
            });
        let resume_el: AnyElement = if !can_resume {
            view()
                .tooltip(kumu_tooltip(
                    "该 Agent 暂不支持恢复历史会话，或尚未记录 Session ID",
                ))
                .child(resume_btn)
                .into_element()
        } else {
            resume_btn.into_element()
        };
        actions =
            actions
                .child(resume_el)
                .child(kumu_button("移除", ButtonVariant::Ghost).on_press(move || {
                    send(&tx_remove, TodoBoardAction::RemoveLink(link_id));
                }));
    }

    let mut card = view()
        .flex_col()
        .w_full()
        .p_2p5()
        .gap_2()
        .rounded(move || active_theme().radius)
        .bg(move || active_theme().card)
        .border_1()
        .border_color(move || active_theme().border)
        .child(
            view()
                .flex_row()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(left)
                .child(actions),
        );

    if needs_attn {
        card = card.border_l(px(3.0)).border_l_color(warning_color);
    }

    if let Some(pid) = pane_id
        && followup_link_id.get() == Some(link_id)
    {
        let tx_send = tx.clone();
        let tx_enter = tx;
        card = card.child(
            view()
                .flex_row()
                .w_full()
                .pt_2()
                .gap_2()
                .border_t_1()
                .border_t_color(move || active_theme().border)
                .child(
                    kumu_input("输入追问内容，回车直接发送到终端…")
                        .flex_1()
                        .h(px(28.0))
                        .value(move || followup_text.get())
                        .on_change(move |v: &str| followup_text.set(v.to_owned()))
                        .on_submit(move |v: &str| {
                            let msg = v.trim().to_string();
                            if !msg.is_empty() {
                                followup_text.set(String::new());
                                followup_link_id.set(None);
                                send(
                                    &tx_enter,
                                    TodoBoardAction::SendFollowup {
                                        todo_id,
                                        pane_id: pid,
                                        text: msg,
                                    },
                                );
                            }
                        }),
                )
                .child(
                    kumu_button("发送", ButtonVariant::Primary).on_press(move || {
                        let msg = followup_text.get_untracked().trim().to_string();
                        if !msg.is_empty() {
                            followup_text.set(String::new());
                            followup_link_id.set(None);
                            send(
                                &tx_send,
                                TodoBoardAction::SendFollowup {
                                    todo_id,
                                    pane_id: pid,
                                    text: msg,
                                },
                            );
                        }
                    }),
                ),
        );
    }

    card.into_element()
}

fn render_detail_body(
    card: &TodoCardSnapshot,
    board: &TodoBoardSnapshot,
    title_draft: Signal<String>,
    desc_draft: Signal<String>,
    followup_link_id: Signal<Option<i64>>,
    followup_text: Signal<String>,
    tx: Option<UnboundedSender<TodoBoardAction>>,
) -> AnyElement {
    let todo_id = card.id;
    let live_count = card.links.iter().filter(|l| l.pane_id.is_some()).count();
    let default_agent = board
        .default_agent
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let default_label = short_agent_name(&default_agent);

    let orig_title = card.title.clone();
    let orig_desc = card.description.clone();
    let tx_close = tx.clone();
    let tx_title = tx.clone();
    let tx_desc = tx.clone();
    let tx_status = tx.clone();
    let tx_del = tx.clone();
    let tx_launch_menu = tx.clone();
    let tx_bind_menu = tx.clone();
    let tx_quick_tab = tx.clone();
    let tx_quick_wt = tx.clone();
    let tx_quick_split = tx.clone();

    let installed_for_menu = board.installed_agents.clone();
    let live_panes_for_menu = board.live_panes.clone();
    let def_a1 = default_agent.clone();
    let def_a2 = default_agent.clone();
    let def_a3 = default_agent;

    let mut sessions_col = view().flex_col().w_full().gap_2();
    if card.links.is_empty() {
        sessions_col = sessions_col.child(
            view()
                .w_full()
                .p_4()
                .items_center()
                .justify_center()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().card)
                .border_1()
                .border_color(move || active_theme().border)
                .child(meta_text(
                    "尚未绑定 Agent 会话 · 点击右侧或上方按钮启动或关联一个或多个 Agent",
                )),
        );
    } else {
        for link in &card.links {
            sessions_col = sessions_col.child(render_link_row(
                todo_id,
                link,
                followup_link_id,
                followup_text,
                tx.clone(),
            ));
        }
    }

    let left_col = scroll()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .border_r_1()
        .border_r_color(move || active_theme().border)
        .child(
            view()
                .flex_col()
                .w_full()
                .p_4()
                .gap_4()
                .child(
                    view()
                        .flex_col()
                        .gap_1()
                        .child(meta_text("标题"))
                        .child(
                            kumu_input("任务标题…")
                                .w_full()
                                .h(px(34.0))
                                .value(move || title_draft.get())
                                .on_change(move |v: &str| title_draft.set(v.to_owned()))
                                .on_blur(move || {
                                    let next = title_draft.get_untracked().trim().to_string();
                                    if !next.is_empty() {
                                        send(
                                            &tx_title,
                                            TodoBoardAction::UpdateTitle {
                                                id: todo_id,
                                                title: next,
                                            },
                                        );
                                    }
                                }),
                        ),
                )
                .child(
                    view()
                        .flex_col()
                        .gap_1()
                        .child(meta_text("任务描述（派发给 Agent 时会作为任务上下文）"))
                        .child(
                            kumu_input("补充验收标准、文件路径或实现思路…")
                                .multiline(true)
                                .w_full()
                                .min_h(px(96.0))
                                .value(move || desc_draft.get())
                                .on_change(move |v: &str| desc_draft.set(v.to_owned()))
                                .on_blur(move || {
                                    send(
                                        &tx_desc,
                                        TodoBoardAction::UpdateDescription {
                                            id: todo_id,
                                            description: desc_draft.get_untracked(),
                                        },
                                    );
                                }),
                        ),
                )
                .child(
                    view()
                        .flex_col()
                        .w_full()
                        .gap_2()
                        .child(
                            view()
                                .flex_row()
                                .w_full()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    text(format!("关联的 Agent & Session ({})", card.links.len()))
                                        .text_size(text_xs_size)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(move || active_theme().muted_foreground),
                                )
                                .child(
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            kumu_button("＋ 启动 Agent ▾", ButtonVariant::Default)
                                                .on_press(move || {
                                                    let mut items = Vec::new();
                                                    for (aid, aname) in &installed_for_menu {
                                                        let tx_t = tx_launch_menu.clone();
                                                        let tx_w = tx_launch_menu.clone();
                                                        let tx_s = tx_launch_menu.clone();
                                                        let a1 = aid.clone();
                                                        let a2 = aid.clone();
                                                        let a3 = aid.clone();
                                                        items.push(MenuItem::submenu(
                                                            aname.clone(),
                                                            [
                                                                MenuItem::new("在新页签启动")
                                                                    .on_select(move || {
                                                                        send(
                                                                            &tx_t,
                                                                            TodoBoardAction::DispatchNewAgent {
                                                                                id: todo_id,
                                                                                agent_id: a1.clone(),
                                                                                target: TodoDispatchTarget::NewTab,
                                                                            },
                                                                        );
                                                                    }),
                                                                MenuItem::new("在新 Worktree 启动")
                                                                    .on_select(move || {
                                                                        send(
                                                                            &tx_w,
                                                                            TodoBoardAction::DispatchNewAgent {
                                                                                id: todo_id,
                                                                                agent_id: a2.clone(),
                                                                                target: TodoDispatchTarget::NewWorktree,
                                                                            },
                                                                        );
                                                                    }),
                                                                MenuItem::new("向右分屏启动")
                                                                    .on_select(move || {
                                                                        send(
                                                                            &tx_s,
                                                                            TodoBoardAction::DispatchNewAgent {
                                                                                id: todo_id,
                                                                                agent_id: a3.clone(),
                                                                                target: TodoDispatchTarget::SplitRight,
                                                                            },
                                                                        );
                                                                    }),
                                                            ],
                                                        ));
                                                    }
                                                    if !items.is_empty() {
                                                        drop(popup_menu(items));
                                                    }
                                                }),
                                        )
                                        .child(
                                            kumu_button("关联已有终端 ▾", ButtonVariant::Ghost)
                                                .disabled(live_panes_for_menu.is_empty())
                                                .on_press(move || {
                                                    let items: Vec<MenuItem> = live_panes_for_menu
                                                        .iter()
                                                        .map(|pane| {
                                                            let p = pane.clone();
                                                            let tx_b = tx_bind_menu.clone();
                                                            let label = format!(
                                                                "{} · {} / {}",
                                                                short_agent_name(&pane.agent),
                                                                pane.project_name,
                                                                pane.tab_name
                                                            );
                                                            MenuItem::new(label).on_select(
                                                                move || {
                                                                    send(
                                                                        &tx_b,
                                                                        TodoBoardAction::BindExistingPane {
                                                                            id: todo_id,
                                                                            pane: p.clone(),
                                                                            send_prompt: false,
                                                                        },
                                                                    );
                                                                },
                                                            )
                                                        })
                                                        .collect();
                                                    if !items.is_empty() {
                                                        drop(popup_menu(items));
                                                    }
                                                }),
                                        ),
                                ),
                        )
                        .child(sessions_col),
                ),
        );

    let meta_row = |k: &'static str, v: String| {
        view()
            .flex_row()
            .w_full()
            .py_1p5()
            .items_center()
            .justify_between()
            .gap_2()
            .border_b_1()
            .border_b_color(move || active_theme().border)
            .child(meta_text(k))
            .child(
                text(v)
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().foreground),
            )
    };

    let right_col = view()
        .flex_col()
        .w(px(270.0))
        .flex_shrink_0()
        .h_full()
        .p_4()
        .gap_4()
        .bg(move || active_theme().secondary.opacity(0.32))
        .child(
            view()
                .flex_col()
                .gap_1p5()
                .child(meta_text("状态"))
                .child(render_status_seg(card.status, move |status| {
                    send(
                        &tx_status,
                        TodoBoardAction::SetStatus {
                            id: todo_id,
                            status,
                        },
                    );
                })),
        )
        .child(
            view()
                .flex_col()
                .p_3()
                .gap_2()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().card)
                .border_1()
                .border_color(move || active_theme().border)
                .child(
                    text("快速派发 Agent")
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || active_theme().foreground),
                )
                .child(
                    kumu_button(
                        format!("▶ {default_label}（新页签）"),
                        ButtonVariant::Primary,
                    )
                    .w_full()
                    .on_press(move || {
                        send(
                            &tx_quick_tab,
                            TodoBoardAction::DispatchNewAgent {
                                id: todo_id,
                                agent_id: def_a1.clone(),
                                target: TodoDispatchTarget::NewTab,
                            },
                        );
                    }),
                )
                .child(
                    kumu_button(
                        format!("⎇ {default_label}（新 Worktree）"),
                        ButtonVariant::Default,
                    )
                    .w_full()
                    .on_press(move || {
                        send(
                            &tx_quick_wt,
                            TodoBoardAction::DispatchNewAgent {
                                id: todo_id,
                                agent_id: def_a2.clone(),
                                target: TodoDispatchTarget::NewWorktree,
                            },
                        );
                    }),
                )
                .child(
                    kumu_button(
                        format!("◫ {default_label}（向右分屏）"),
                        ButtonVariant::Ghost,
                    )
                    .w_full()
                    .on_press(move || {
                        send(
                            &tx_quick_split,
                            TodoBoardAction::DispatchNewAgent {
                                id: todo_id,
                                agent_id: def_a3.clone(),
                                target: TodoDispatchTarget::SplitRight,
                            },
                        );
                    }),
                ),
        )
        .child(
            view()
                .flex_col()
                .px_3()
                .py_1()
                .rounded(move || active_theme().radius)
                .bg(move || active_theme().card)
                .border_1()
                .border_color(move || active_theme().border)
                .child(meta_row("所属项目", board.board_name.clone()))
                .child(meta_row(
                    "关联会话",
                    format!("{} 个（{} 运行中）", card.links.len(), live_count),
                ))
                .child(meta_row("创建时间", card.created_at.clone()))
                .child(meta_row("更新时间", card.updated_at.clone())),
        )
        .child(view().flex_1())
        .child(view().flex_row().justify_end().child(
            kumu_button("删除任务", ButtonVariant::Danger).on_press(move || {
                send(&tx_del, TodoBoardAction::DeleteTodo(todo_id));
            }),
        ));

    view()
        .flex_col()
        .w(px(840.0))
        .max_w(px(880.0))
        .h(px(580.0))
        .max_h(px(640.0))
        .rounded(move || active_theme().radius)
        .bg(move || active_theme().background)
        .border_1()
        .border_color(move || active_theme().border)
        .overflow_hidden()
        .child(
            view()
                .flex_row()
                .flex_shrink_0()
                .h(px(42.0))
                .px_4()
                .items_center()
                .justify_between()
                .bg(move || active_theme().sidebar)
                .border_b_1()
                .border_b_color(move || active_theme().border)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(meta_text(format!("{} / #{}", board.board_name, todo_id)))
                        .child(meta_text(format!("· {} 个会话", card.links.len()))),
                )
                .child(kumu_icon_button(
                    "关闭 Esc",
                    icon_svg("icons/close.svg", 14.0)
                        .text_color(move || active_theme().muted_foreground),
                    ControlTone::Default,
                    move || {
                        let next_title = title_draft.get_untracked().trim().to_string();
                        if !next_title.is_empty() && next_title != orig_title {
                            send(
                                &tx_close,
                                TodoBoardAction::UpdateTitle {
                                    id: todo_id,
                                    title: next_title,
                                },
                            );
                        }
                        let next_desc = desc_draft.get_untracked();
                        if next_desc != orig_desc {
                            send(
                                &tx_close,
                                TodoBoardAction::UpdateDescription {
                                    id: todo_id,
                                    description: next_desc,
                                },
                            );
                        }
                        send(&tx_close, TodoBoardAction::CloseModal);
                    },
                )),
        )
        .child(
            view()
                .flex_row()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(left_col)
                .child(right_col),
        )
        .into_element()
}

#[component]
fn TodoModalRoot(props: TodoModalRootProps) -> impl IntoElement {
    let mount = MOUNTS.take(props.mount_id.get_untracked());
    let (initial, events_rx, actions_tx) = match mount {
        Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
        None => {
            return view().size_full().into_element();
        }
    };

    let state = signal(initial.clone());
    let create_title = signal(String::new());
    let create_desc = signal(String::new());
    let create_status = signal(match &initial {
        TodoModalState::Create { initial_status, .. } => *initial_status,
        TodoModalState::Detail { .. } => TodoStatus::Todo,
    });
    let detail_title = signal(String::new());
    let detail_desc = signal(String::new());
    let detail_loaded_id = stored(Option::<i64>::None);
    let followup_link_id = signal(Option::<i64>::None);
    let followup_text = signal(String::new());

    let flush_detail_and_close = move |tx: &Option<UnboundedSender<TodoBoardAction>>| {
        if let TodoModalState::Detail { todo_id, board } = state.get_untracked()
            && let Some(card) = board.todos.iter().find(|t| t.id == todo_id)
        {
            let next_title = detail_title.get_untracked().trim().to_string();
            if !next_title.is_empty() && next_title != card.title {
                send(
                    tx,
                    TodoBoardAction::UpdateTitle {
                        id: todo_id,
                        title: next_title,
                    },
                );
            }
            let next_desc = detail_desc.get_untracked();
            if next_desc != card.description {
                send(
                    tx,
                    TodoBoardAction::UpdateDescription {
                        id: todo_id,
                        description: next_desc,
                    },
                );
            }
        }
        send(tx, TodoBoardAction::CloseModal);
    };

    if let Some(mut rx) = events_rx {
        let tx_close_ev = actions_tx.clone();
        spawn(async move {
            while let Some(ev) = rx.next().await {
                match ev {
                    TodoModalEvent::State(next) => {
                        // Keep `create_status` user-controlled after initial mount so
                        // background Agent status updates never reset the selected column.
                        state.set(next);
                    }
                    TodoModalEvent::RequestClose => {
                        flush_detail_and_close(&tx_close_ev);
                    }
                }
            }
        });
    }

    let tx_bg = actions_tx.clone();
    let tx_esc = actions_tx.clone();
    let tx_body = actions_tx;

    view()
        .relative()
        .size_full()
        .items_center()
        .justify_center()
        .p_6()
        .on_key_down(move |ev: &nsh_core::KeyEvent| {
            if ev.key == "Escape" {
                flush_detail_and_close(&tx_esc);
            }
        })
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
                .app_class("bg-black/35")
                .on_press(move || flush_detail_and_close(&tx_bg)),
        )
        .child(dynamic(move || match state.get() {
            TodoModalState::Create { board, .. } => {
                let tx_close = tx_body.clone();
                let tx_cancel = tx_body.clone();
                let tx_create = tx_body.clone();
                let tx_create_run = tx_body.clone();
                let default_label = board
                    .default_agent
                    .as_deref()
                    .map(short_agent_name)
                    .unwrap_or_else(|| "Agent".to_string());
                let cur_status = create_status.get();

                view()
                    .flex_col()
                    .w(px(540.0))
                    .max_w(px(580.0))
                    .rounded(move || active_theme().radius)
                    .bg(move || active_theme().background)
                    .border_1()
                    .border_color(move || active_theme().border)
                    .overflow_hidden()
                    .child(
                        view()
                            .flex_row()
                            .h(px(42.0))
                            .px_4()
                            .items_center()
                            .justify_between()
                            .bg(move || active_theme().sidebar)
                            .border_b_1()
                            .border_b_color(move || active_theme().border)
                            .child(
                                text(format!("{} / 新建任务", board.board_name))
                                    .text_size(text_sm_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(move || active_theme().foreground),
                            )
                            .child(kumu_icon_button(
                                "关闭 Esc",
                                icon_svg("icons/close.svg", 14.0)
                                    .text_color(move || active_theme().muted_foreground),
                                ControlTone::Default,
                                move || send(&tx_close, TodoBoardAction::CloseModal),
                            )),
                    )
                    .child(
                        view()
                            .flex_col()
                            .p_4()
                            .gap_3()
                            .child(
                                view()
                                    .flex_col()
                                    .gap_1()
                                    .child(meta_text("任务标题"))
                                    .child(
                                        kumu_input("输入任务标题…")
                                            .auto_focus(true)
                                            .w_full()
                                            .h(px(34.0))
                                            .value(move || create_title.get())
                                            .on_change(move |v: &str| {
                                                create_title.set(v.to_owned());
                                            }),
                                    ),
                            )
                            .child(
                                view()
                                    .flex_col()
                                    .gap_1()
                                    .child(meta_text("任务描述（可选，派发给 Agent 时作为上下文）"))
                                    .child(
                                        kumu_input("补充验收标准、文件路径或实现思路…")
                                            .multiline(true)
                                            .w_full()
                                            .min_h(px(96.0))
                                            .value(move || create_desc.get())
                                            .on_change(move |v: &str| {
                                                create_desc.set(v.to_owned());
                                            }),
                                    ),
                            )
                            .child(view().flex_col().gap_1().child(meta_text("初始列")).child(
                                render_status_seg(cur_status, move |s| {
                                    create_status.set(s);
                                }),
                            ))
                            .child(
                                view()
                                    .flex_row()
                                    .pt_2()
                                    .gap_2()
                                    .justify_end()
                                    .border_t_1()
                                    .border_t_color(move || active_theme().border)
                                    .child(kumu_button("取消", ButtonVariant::Ghost).on_press(
                                        move || send(&tx_cancel, TodoBoardAction::CloseModal),
                                    ))
                                    .child(kumu_button("仅创建", ButtonVariant::Default).on_press(
                                        move || {
                                            let title =
                                                create_title.get_untracked().trim().to_string();
                                            if !title.is_empty() {
                                                send(
                                                    &tx_create,
                                                    TodoBoardAction::CreateTodo {
                                                        title,
                                                        description: create_desc.get_untracked(),
                                                        status: create_status.get_untracked(),
                                                        dispatch_default: false,
                                                    },
                                                );
                                            }
                                        },
                                    ))
                                    .child(
                                        kumu_button(
                                            format!("▶ 创建并启动 {default_label}"),
                                            ButtonVariant::Primary,
                                        )
                                        .on_press(
                                            move || {
                                                let title =
                                                    create_title.get_untracked().trim().to_string();
                                                if !title.is_empty() {
                                                    send(
                                                        &tx_create_run,
                                                        TodoBoardAction::CreateTodo {
                                                            title,
                                                            description: create_desc
                                                                .get_untracked(),
                                                            status: create_status.get_untracked(),
                                                            dispatch_default: true,
                                                        },
                                                    );
                                                }
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .into_element()
            }
            TodoModalState::Detail { todo_id, board } => {
                let Some(card) = board.todos.iter().find(|t| t.id == todo_id) else {
                    return view()
                        .p_4()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().background)
                        .child(body_text("任务不存在或已删除"))
                        .into_element();
                };
                if detail_loaded_id.get() != Some(todo_id) {
                    detail_loaded_id.replace(Some(todo_id));
                    detail_title.set(card.title.clone());
                    detail_desc.set(card.description.clone());
                }
                render_detail_body(
                    card,
                    &board,
                    detail_title,
                    detail_desc,
                    followup_link_id,
                    followup_text,
                    tx_body.clone(),
                )
            }
        }))
        .into_element()
}

/// Active full-window TODO Create/Detail modal mounted over `KumuApp`.
pub(in crate::ui::home) struct ActiveTodoModal {
    mount_id: u64,
    pub(in crate::ui::home) kind: TodoModalKind,
    pub(in crate::ui::home) app_view: Entity<AppView>,
    events_tx: UnboundedSender<TodoModalEvent>,
    _actions_task: Task<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui::home) enum TodoModalKind {
    Create(TodoStatus),
    Detail(i64),
}

impl Drop for ActiveTodoModal {
    fn drop(&mut self) {
        let _ = MOUNTS.take(self.mount_id);
    }
}

impl ActiveTodoModal {
    pub(in crate::ui::home) fn open<V: 'static>(
        kind: TodoModalKind,
        board: TodoBoardSnapshot,
        on_action: impl Fn(&mut V, TodoBoardAction, &mut Window, &mut Context<V>) + 'static,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        let initial = match kind {
            TodoModalKind::Create(initial_status) => TodoModalState::Create {
                initial_status,
                board,
            },
            TodoModalKind::Detail(todo_id) => TodoModalState::Detail { todo_id, board },
        };
        let (events_tx, events_rx) = unbounded();
        let (actions_tx, mut actions_rx) = unbounded();
        let mount_id = MOUNTS.register(ModalMount {
            initial,
            events_rx,
            actions_tx,
        });

        let theme = KumuTheme::global(cx);
        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu TODO".to_owned(),
                // Keep the overlay surface transparent so the underlying Kanban
                // board and sidebar stay visible behind the dimmed backdrop.
                transparent: true,
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<TodoModalRoot>(
                move || TodoModalRootProps {
                    mount_id: mount_id.into(),
                },
            )));
            view
        });

        let actions_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let ok = this.update_in(cx, |view, window, cx| {
                    on_action(view, action, window, cx);
                });
                if ok.is_err() {
                    break;
                }
            }
        });

        Self {
            mount_id,
            kind,
            app_view,
            events_tx,
            _actions_task: actions_task,
        }
    }

    pub(in crate::ui::home) fn sync_board(&self, board: TodoBoardSnapshot) {
        let next = match self.kind {
            TodoModalKind::Create(initial_status) => TodoModalState::Create {
                initial_status,
                board,
            },
            TodoModalKind::Detail(todo_id) => TodoModalState::Detail { todo_id, board },
        };
        let _ = self.events_tx.unbounded_send(TodoModalEvent::State(next));
    }

    /// Ask the mounted modal to flush any unsaved Detail drafts and emit `CloseModal`.
    pub(in crate::ui::home) fn request_close(&self) -> bool {
        self.events_tx
            .unbounded_send(TodoModalEvent::RequestClose)
            .is_ok()
    }

    pub(in crate::ui::home) fn sync_theme(&self, theme: &KumuTheme, cx: &mut gpui::App) {
        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        self.app_view.update(cx, |view, cx| {
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
        });
    }
}
