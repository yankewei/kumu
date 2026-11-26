//! Inline TODO detail editor (`← 返回看板`) rendered inside the right-hand panel.

use std::collections::HashMap;

use futures::channel::mpsc::UnboundedSender;
use nsh_base::{input, theme as active_theme};
use nsh_core::element::{AccessibilityRole, scroll};
use nsh_core::prelude::*;

use super::dispatch_picker::DispatchPickerState;
use super::{render_notice_banner, send_action};
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoStatus};
use crate::nsh_ui::assets::{agent_logo, icon_svg};
use crate::nsh_ui::components::{ButtonVariant, kumu_button, meta_text};
use crate::nsh_ui::theme::{link_color, success_color, text_xs_size, warning_color};
use crate::plugins::AgentPaneInfo;
use crate::plugins::todo::{
    TodoAction, TodoBoardSnapshot, agent_logo_path, pane_disambiguation, pane_display_title,
    pane_location_label, short_agent_name,
};

#[derive(Clone, Copy)]
pub(super) struct DetailSignals {
    pub notice: Signal<Option<(String, bool)>>,
    pub id: Signal<Option<i64>>,
    pub title: Signal<String>,
    pub desc: Signal<String>,
    pub followup: Signal<String>,
}

impl DetailSignals {
    fn close(self) {
        self.id.set(None);
        self.notice.set(None);
    }
}

pub(super) fn render_detail_view(
    tx: Option<UnboundedSender<TodoAction>>,
    signals: DetailSignals,
    picker: DispatchPickerState,
    item: TodoItem,
    snap: TodoBoardSnapshot,
) -> View {
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

    let header = render_detail_header(tx.clone(), signals, &item);
    let status_seg = render_status_segmented(tx.clone(), id, item.status);
    let agent_section = render_agent_section(
        tx.clone(),
        signals.followup,
        picker,
        &item,
        &snap,
        live_pane,
        live_state,
        needs_review,
    );
    let footer_cta = render_footer_cta(tx.clone(), signals, id, item.status, needs_review);

    let tx_title = tx.clone();
    let tx_desc = tx;
    let body = scroll().flex_1().child(
        view()
            .flex_col()
            .w_full()
            .p_3()
            .gap_3()
            .child(render_notice_banner(signals.notice))
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
                            .value(move || signals.title.get())
                            .on_change(move |v: &str| {
                                signals.title.set(v.to_owned());
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
                            .value(move || signals.desc.get())
                            .on_change(move |v: &str| {
                                signals.desc.set(v.to_owned());
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
}

fn render_detail_header(
    tx: Option<UnboundedSender<TodoAction>>,
    signals: DetailSignals,
    item: &TodoItem,
) -> View {
    let id = item.id;
    view()
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
                .on_press(move || signals.close())
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
                    signals.close();
                    send_action(&tx, TodoAction::DeleteItem(id));
                })
                .child(
                    icon_svg("icons/trash.svg", 13.0)
                        .text_color(move || active_theme().muted_foreground),
                ),
        )
}

fn render_status_segmented(
    tx: Option<UnboundedSender<TodoAction>>,
    id: i64,
    current_status: TodoStatus,
) -> View {
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
        let active = current_status == status;
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
            seg_btn = seg_btn.hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.4)));
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
    status_seg
}

#[allow(clippy::too_many_arguments)]
fn render_agent_section(
    tx: Option<UnboundedSender<TodoAction>>,
    detail_followup: Signal<String>,
    picker: DispatchPickerState,
    item: &TodoItem,
    snap: &TodoBoardSnapshot,
    live_pane: Option<AgentPaneInfo>,
    live_state: Option<AgentState>,
    needs_review: bool,
) -> AnyElement {
    let id = item.id;
    if let Some(pane) = live_pane {
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
        let (multi_project, tab_counts) = pane_disambiguation(&snap.live_panes);
        let multi_in_tab = tab_counts
            .get(&(pane.project_id, pane.tab_name.clone()))
            .copied()
            .unwrap_or(0)
            > 1;
        let location_label = pane_location_label(&pane, multi_project, multi_in_tab);
        let session_title = pane_display_title(&pane)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}（新会话）", short_agent_name(&pane.agent)));
        let logo_path = agent_logo_path(&pane.agent);

        let tx_jump = tx.clone();
        let tx_unbind = tx.clone();
        let tx_followup_submit = tx.clone();
        let tx_followup_btn = tx;

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
                                        view().w(px(6.0)).h(px(6.0)).rounded(px(3.0)).bg(pill_fg),
                                    )
                                    .child(
                                        text(pill_label)
                                            .text_size(text_xs_size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(pill_fg),
                                    ),
                            )
                            .child(view().flex_1())
                            .child(meta_text(location_label)),
                    )
                    .child(
                        view()
                            .flex_row()
                            .w_full()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .gap_1p5()
                            .rounded(px(5.0))
                            .bg(move || active_theme().sidebar.opacity(0.75))
                            .items_center()
                            .child(agent_logo(logo_path, 14.0))
                            .child(
                                text(session_title)
                                    .flex_1()
                                    .min_w_0()
                                    .app_class("truncate")
                                    .text_size(text_xs_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(move || active_theme().foreground),
                            ),
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
                            .child(kumu_button("解绑", ButtonVariant::Ghost).on_press(move || {
                                send_action(&tx_unbind, TodoAction::UnbindAgent(id));
                            })),
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
        let tx_default = tx;
        let has_live_panes = !snap.live_panes.is_empty();

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
                            .on_press(move || picker.open(id, has_live_panes))
                            .child(
                                icon_svg("icons/chevron-down.svg", 13.0)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    ),
            )
            .into_element()
    }
}

fn render_footer_cta(
    tx: Option<UnboundedSender<TodoAction>>,
    signals: DetailSignals,
    id: i64,
    status: TodoStatus,
    needs_review: bool,
) -> AnyElement {
    if status == TodoStatus::Done {
        kumu_button("重新打开（移回待办）", ButtonVariant::Ghost)
            .w_full()
            .on_press(move || {
                send_action(
                    &tx,
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
                    &tx,
                    TodoAction::SetStatus {
                        id,
                        status: TodoStatus::Done,
                    },
                );
                signals.close();
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
                    &tx,
                    TodoAction::SetStatus {
                        id,
                        status: TodoStatus::Done,
                    },
                );
                signals.close();
            })
            .into_element()
    }
}
