//! Custom Pure-Rust NSH popover for dispatching a TODO card to an existing
//! Agent pane (with logo, conversation title, tab/worktree location & status)
//! or launching a fresh Agent pane (split / new tab / new worktree).

use futures::channel::mpsc::UnboundedSender;
use nsh_base::theme as active_theme;
use nsh_core::element::{AccessibilityRole, scroll};
use nsh_core::prelude::*;

use super::send_action;
use crate::agent::AgentState;
use crate::nsh_ui::assets::{agent_logo, icon_svg};
use crate::nsh_ui::components::{ButtonVariant, kumu_button, meta_text};
use crate::nsh_ui::theme::{link_color, success_color, text_xs_size, warning_color};
use crate::plugins::AgentPaneInfo;
use crate::plugins::todo::{
    TodoAction, TodoBoardSnapshot, TodoDispatchTarget, agent_logo_path, pane_disambiguation,
    pane_display_title, pane_location_label, short_agent_name,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DispatchPickerTab {
    Existing,
    NewAgent,
}

#[derive(Clone, Copy)]
pub(super) struct DispatchPickerState {
    pub todo_id: Signal<Option<i64>>,
    pub tab: Signal<DispatchPickerTab>,
    pub send_prompt: Signal<bool>,
}

impl DispatchPickerState {
    pub(super) fn new() -> Self {
        Self {
            todo_id: signal(None),
            tab: signal(DispatchPickerTab::Existing),
            send_prompt: signal(true),
        }
    }

    pub(super) fn open(self, id: i64, has_live_panes: bool) {
        self.tab.set(if has_live_panes {
            DispatchPickerTab::Existing
        } else {
            DispatchPickerTab::NewAgent
        });
        self.send_prompt.set(true);
        self.todo_id.set(Some(id));
    }

    pub(super) fn close(self) {
        self.todo_id.set(None);
    }
}

fn state_dot_color(state: Option<AgentState>) -> AdaptiveColor {
    match state {
        Some(AgentState::Running) => link_color(),
        Some(AgentState::Waiting) => warning_color(),
        Some(AgentState::Completed) => success_color(),
        Some(AgentState::Failed) => active_theme().destructive,
        None => active_theme().muted_foreground,
    }
}

pub(super) fn render_dispatch_picker_overlay(
    tx: Option<UnboundedSender<TodoAction>>,
    picker: DispatchPickerState,
    snap: &TodoBoardSnapshot,
) -> AnyElement {
    let Some(todo_id) = picker.todo_id.get() else {
        return view().into_element();
    };
    let Some(todo) = snap.todos.iter().find(|t| t.id == todo_id) else {
        return view().into_element();
    };

    let active_tab = picker.tab.get();
    let live_count = snap.live_panes.len();
    let todo_title = todo.title.clone();

    let header = view()
        .flex_row()
        .w_full()
        .px_2p5()
        .py_1p5()
        .gap_1p5()
        .items_center()
        .border_b_1()
        .border_b_color(move || active_theme().sidebar_border)
        .child(
            text(format!("派发「{todo_title}」"))
                .flex_1()
                .min_w_0()
                .app_class("truncate")
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().popover_foreground),
        )
        .child(
            view()
                .role(AccessibilityRole::Button)
                .label("关闭派发选择")
                .p_0p5()
                .rounded(px(4.0))
                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                .on_press(move || picker.close())
                .child(
                    icon_svg("icons/x.svg", 12.0)
                        .text_color(move || active_theme().muted_foreground),
                ),
        );

    let tabs_bar = view()
        .flex_row()
        .w_full()
        .p_1()
        .gap_0p5()
        .bg(move || active_theme().sidebar)
        .border_b_1()
        .border_b_color(move || active_theme().sidebar_border)
        .child(render_tab_button(
            format!("已有窗格 ({live_count})"),
            active_tab == DispatchPickerTab::Existing,
            move || picker.tab.set(DispatchPickerTab::Existing),
        ))
        .child(render_tab_button(
            "启动新 Agent".to_string(),
            active_tab == DispatchPickerTab::NewAgent,
            move || picker.tab.set(DispatchPickerTab::NewAgent),
        ));

    let body = match active_tab {
        DispatchPickerTab::Existing => render_existing_tab(tx, picker, todo_id, snap),
        DispatchPickerTab::NewAgent => render_new_agent_tab(tx, picker, todo_id, snap),
    };

    let card = view()
        .relative()
        .flex_col()
        .w_full()
        .max_h(px(360.0))
        .rounded(px(8.0))
        .bg(move || active_theme().popover)
        .border_1()
        .border_color(move || active_theme().border)
        .app_class("shadow-lg")
        .overflow_hidden()
        .child(header)
        .child(tabs_bar)
        .child(body);

    view()
        .absolute()
        .inset_0()
        .flex_col()
        .justify_center()
        .px_2()
        .py_3()
        .child(
            view()
                .absolute()
                .inset_0()
                .bg(move || active_theme().background.opacity(0.45))
                .on_press(move || picker.close()),
        )
        .child(card)
        .into_element()
}

fn render_tab_button(label: String, active: bool, on_press: impl Fn() + 'static) -> View {
    let mut btn = view()
        .role(AccessibilityRole::Button)
        .flex_1()
        .h(px(24.0))
        .px_1p5()
        .rounded(px(5.0))
        .items_center()
        .justify_center()
        .app_class("cursor-pointer")
        .on_press(on_press);
    if active {
        btn = btn.bg(move || active_theme().background);
    } else {
        btn = btn.hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)));
    }
    btn.child(
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
    )
}

fn render_existing_tab(
    tx: Option<UnboundedSender<TodoAction>>,
    picker: DispatchPickerState,
    todo_id: i64,
    snap: &TodoBoardSnapshot,
) -> AnyElement {
    if snap.live_panes.is_empty() {
        return view()
            .flex_col()
            .w_full()
            .px_3()
            .py_4()
            .gap_2()
            .items_center()
            .child(meta_text("当前仓库暂无运行中的 Agent 窗格"))
            .child(
                kumu_button("去启动新 Agent", ButtonVariant::Ghost)
                    .on_press(move || picker.tab.set(DispatchPickerTab::NewAgent)),
            )
            .into_element();
    }

    let (multi_project, tab_counts) = pane_disambiguation(&snap.live_panes);
    let mut list = view().flex_col().w_full().p_1p5().gap_0p5();
    for pane in &snap.live_panes {
        let multi_in_tab = tab_counts
            .get(&(pane.project_id, pane.tab_name.clone()))
            .copied()
            .unwrap_or(0)
            > 1;
        list = list.child(render_existing_pane_row(
            tx.clone(),
            picker,
            todo_id,
            pane,
            multi_project,
            multi_in_tab,
        ));
    }

    let send_prompt = picker.send_prompt.get();
    let footer = view()
        .role(AccessibilityRole::Button)
        .label("切换是否同时发送任务 Prompt")
        .flex_row()
        .flex_shrink_0()
        .w_full()
        .px_2p5()
        .py_1p5()
        .gap_1p5()
        .items_center()
        .border_t_1()
        .border_t_color(move || active_theme().sidebar_border)
        .bg(move || active_theme().sidebar.opacity(0.65))
        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
        .on_press(move || picker.send_prompt.update(|v| *v = !*v))
        .child(
            view()
                .w(px(13.0))
                .h(px(13.0))
                .rounded(px(3.0))
                .border_1()
                .border_color(move || {
                    if send_prompt {
                        link_color()
                    } else {
                        active_theme().input
                    }
                })
                .bg(move || {
                    if send_prompt {
                        link_color()
                    } else {
                        active_theme().background
                    }
                })
                .items_center()
                .justify_center()
                .when(send_prompt, |s| {
                    s.child(
                        icon_svg("icons/check.svg", 9.0)
                            .text_color(move || active_theme().primary_foreground),
                    )
                }),
        )
        .child(
            text("同时发送任务 Prompt")
                .text_size(text_xs_size)
                .text_color(move || active_theme().foreground),
        )
        .child(view().flex_1())
        .child(meta_text(if send_prompt {
            "点击即发送"
        } else {
            "仅关联窗格"
        }));

    view()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .child(scroll().flex_1().child(list))
        .child(footer)
        .into_element()
}

fn render_existing_pane_row(
    tx: Option<UnboundedSender<TodoAction>>,
    picker: DispatchPickerState,
    todo_id: i64,
    pane: &AgentPaneInfo,
    multi_project: bool,
    multi_in_tab: bool,
) -> AnyElement {
    let agent = short_agent_name(&pane.agent);
    let primary_title = pane_display_title(pane)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{agent}（新会话）"));
    let location = pane_location_label(pane, multi_project, multi_in_tab);
    let status = pane.state.map_or("空闲", AgentState::label);
    let subtitle = format!("{agent} · {location} · {status}");
    let state = pane.state;
    let focused = pane.focused;
    let pane_clone = pane.clone();

    let mut top_row = view()
        .flex_row()
        .w_full()
        .min_w_0()
        .items_center()
        .gap_1p5()
        .child(
            text(primary_title.clone())
                .flex_1()
                .min_w_0()
                .app_class("truncate")
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().popover_foreground),
        );

    if focused {
        top_row = top_row.child(
            view()
                .flex_shrink_0()
                .px_1()
                .rounded(px(4.0))
                .bg(move || link_color().opacity(0.14))
                .child(
                    text("当前")
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(link_color),
                ),
        );
    }

    view()
        .role(AccessibilityRole::Button)
        .label(format!("{primary_title} · {subtitle}"))
        .flex_row()
        .w_full()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_start()
        .rounded(move || active_theme().radius)
        .app_class("cursor-pointer")
        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.75)))
        .on_press(move || {
            let send_prompt = picker.send_prompt.get_untracked();
            picker.close();
            send_action(
                &tx,
                TodoAction::BindExistingPane {
                    id: todo_id,
                    pane: pane_clone.clone(),
                    send_prompt,
                },
            );
        })
        .child(
            view()
                .pt_0p5()
                .flex_shrink_0()
                .child(agent_logo(agent_logo_path(&pane.agent), 16.0)),
        )
        .child(
            view()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(top_row)
                .child(
                    view()
                        .flex_row()
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_1()
                        .child(
                            view()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded(px(3.0))
                                .flex_shrink_0()
                                .bg(move || state_dot_color(state)),
                        )
                        .child(
                            text(subtitle)
                                .flex_1()
                                .min_w_0()
                                .app_class("truncate")
                                .text_size(text_xs_size)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                ),
        )
        .into_element()
}

fn render_new_agent_tab(
    tx: Option<UnboundedSender<TodoAction>>,
    picker: DispatchPickerState,
    todo_id: i64,
    snap: &TodoBoardSnapshot,
) -> AnyElement {
    if snap.installed_agents.is_empty() {
        return view()
            .flex_col()
            .w_full()
            .px_3()
            .py_4()
            .items_center()
            .child(meta_text("未检测到已安装的 Agent"))
            .into_element();
    }

    let mut list = view().flex_col().w_full().p_1p5().gap_0p5();
    for (agent_id, _full_name) in &snap.installed_agents {
        let is_default = snap.default_agent.as_deref() == Some(agent_id.as_str());
        let display_name = short_agent_name(agent_id);
        let id_split = agent_id.clone();
        let id_tab = agent_id.clone();
        let id_wt = agent_id.clone();
        let tx_split = tx.clone();
        let tx_tab = tx.clone();
        let tx_wt = tx.clone();

        let mut name_row = view()
            .flex_row()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_1()
            .child(
                text(display_name.clone())
                    .app_class("truncate")
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().popover_foreground),
            );
        if is_default {
            name_row = name_row.child(meta_text("默认"));
        }

        let make_chip = |chip_label: &'static str,
                         accessible: String,
                         target: TodoDispatchTarget,
                         aid: String,
                         tx_chip: Option<UnboundedSender<TodoAction>>| {
            view()
                .role(AccessibilityRole::Button)
                .label(accessible)
                .flex_shrink_0()
                .px_1p5()
                .py_0p5()
                .rounded(px(4.0))
                .bg(move || active_theme().sidebar)
                .border_1()
                .border_color(move || active_theme().border)
                .hover(|s| {
                    s.bg(move || active_theme().accent)
                        .border_color(move || link_color().opacity(0.45))
                })
                .on_press(move || {
                    picker.close();
                    send_action(
                        &tx_chip,
                        TodoAction::DispatchNewAgent {
                            id: todo_id,
                            agent_id: aid.clone(),
                            target,
                        },
                    );
                })
                .child(
                    text(chip_label)
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground),
                )
        };

        list = list.child(
            view()
                .flex_row()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_1p5()
                .items_center()
                .rounded(move || active_theme().radius)
                .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.55)))
                .child(agent_logo(agent_logo_path(agent_id), 15.0))
                .child(name_row)
                .child(
                    view()
                        .flex_row()
                        .flex_shrink_0()
                        .gap_1()
                        .child(make_chip(
                            "分屏",
                            format!("分屏启动 {display_name}"),
                            TodoDispatchTarget::SplitRight,
                            id_split,
                            tx_split,
                        ))
                        .child(make_chip(
                            "新页签",
                            format!("在新页签中启动 {display_name}"),
                            TodoDispatchTarget::NewTab,
                            id_tab,
                            tx_tab,
                        ))
                        .child(make_chip(
                            "Worktree",
                            format!("在新建 Worktree 中启动 {display_name}"),
                            TodoDispatchTarget::NewWorktree,
                            id_wt,
                            tx_wt,
                        )),
                ),
        );
    }

    scroll().flex_1().child(list).into_element()
}
