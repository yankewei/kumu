use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{DragPreview, DraggedTodo, TodoPanel, short_agent_name};
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoStatus};
use crate::plugins::AgentPaneInfo;

impl TodoPanel {
    /// Calm in-progress card: clicking a linked card jumps directly to its
    /// Agent pane; cards needing human action get a subtle 3px left accent strip
    /// and a concise right-side CTA (`确认` / `去回复`).
    pub(super) fn render_in_progress_card(
        &self,
        item: &TodoItem,
        live_pane: Option<&AgentPaneInfo>,
        live_panes: &[AgentPaneInfo],
        default_agent: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, fg, muted, radius) = (
            theme.background,
            theme.border,
            theme.foreground,
            theme.muted_foreground,
            theme.radius,
        );
        let live_state = live_pane.and_then(|pane| pane.state);
        let needs_review = item.awaiting_review || live_state == Some(AgentState::Completed);

        let accent: Option<Hsla> = if live_state == Some(AgentState::Waiting) {
            Some(theme.warning)
        } else if needs_review {
            Some(theme.success)
        } else if live_state == Some(AgentState::Failed) {
            Some(theme.danger)
        } else {
            None
        };

        let (pill_bg, pill_fg, pill_icon, pill_text) = match (live_pane, live_state) {
            (Some(pane), Some(AgentState::Waiting)) => (
                theme.warning.opacity(0.16),
                theme.warning,
                None,
                format!("{} · 等待输入", short_agent_name(&pane.agent)),
            ),
            (Some(pane), Some(AgentState::Completed)) => (
                theme.success.opacity(0.16),
                theme.success,
                Some(IconName::Check),
                format!("{} · 待确认", short_agent_name(&pane.agent)),
            ),
            (Some(pane), _) if needs_review => (
                theme.success.opacity(0.16),
                theme.success,
                Some(IconName::Check),
                format!("{} · 待确认", short_agent_name(&pane.agent)),
            ),
            (Some(pane), Some(AgentState::Failed)) => (
                theme.danger.opacity(0.16),
                theme.danger,
                Some(IconName::TriangleAlert),
                format!("{} · 失败", short_agent_name(&pane.agent)),
            ),
            (Some(pane), Some(AgentState::Running)) => (
                theme.link.opacity(0.14),
                theme.link,
                Some(IconName::LoaderCircle),
                format!("{} · 运行中", short_agent_name(&pane.agent)),
            ),
            (Some(pane), None) => (
                theme.sidebar_accent.opacity(0.7),
                muted,
                None,
                format!("{} · 空闲", short_agent_name(&pane.agent)),
            ),
            (None, _) if needs_review => (
                theme.success.opacity(0.16),
                theme.success,
                Some(IconName::Check),
                item.agent_id
                    .as_deref()
                    .map(|a| format!("{} · 待确认", short_agent_name(a)))
                    .unwrap_or_else(|| "待确认".to_string()),
            ),
            (None, _) => (
                theme.sidebar_accent.opacity(0.65),
                muted,
                None,
                item.agent_id
                    .as_deref()
                    .map(|a| format!("{} · 窗格已关闭", short_agent_name(a)))
                    .unwrap_or_else(|| "未关联 Agent".to_string()),
            ),
        };

        let card_item = item.clone();
        let jump_pane = live_pane.map(|p| p.pane);
        let id = item.id;
        let drag_title = item.title.clone();
        let panel_weak = cx.entity().downgrade();
        let menu_item = item.clone();
        let menu_panes = live_panes.to_vec();
        let menu_default = default_agent.map(str::to_string);

        let more_item = item.clone();
        let more_btn = div()
            .id(format!("todo-more-{id}"))
            .flex_none()
            .p_0p5()
            .rounded(radius)
            .cursor_pointer()
            .text_color(muted)
            .hover(move |style| style.bg(theme.accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_detail(&more_item, window, cx);
            }))
            .child(Icon::new(IconName::Ellipsis).with_size(px(13.)));

        let pill = h_flex()
            .flex_shrink_1()
            .min_w_0()
            .h(px(20.))
            .px_1p5()
            .gap_1()
            .rounded(px(5.))
            .bg(pill_bg)
            .items_center()
            .child(match pill_icon {
                Some(icon) => Icon::new(icon)
                    .with_size(px(11.))
                    .text_color(pill_fg)
                    .into_any_element(),
                None => div()
                    .flex_none()
                    .size(px(6.))
                    .rounded_full()
                    .bg(pill_fg)
                    .into_any_element(),
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_medium()
                    .text_color(pill_fg)
                    .child(pill_text),
            );

        let right_cta: Option<AnyElement> = if needs_review {
            let success = theme.success;
            Some(
                div()
                    .id(format!("todo-confirm-{id}"))
                    .flex_none()
                    .h(px(20.))
                    .px_2()
                    .rounded(px(5.))
                    .bg(success.opacity(0.16))
                    .hover(move |style| style.bg(success.opacity(0.28)))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .text_xs()
                    .font_semibold()
                    .text_color(success)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.set_status(id, TodoStatus::Done, cx);
                    }))
                    .child("确认")
                    .into_any_element(),
            )
        } else if live_state == Some(AgentState::Waiting) {
            let warning = theme.warning;
            Some(
                div()
                    .id(format!("todo-reply-{id}"))
                    .flex_none()
                    .h(px(20.))
                    .px_2()
                    .rounded(px(5.))
                    .bg(warning.opacity(0.16))
                    .hover(move |style| style.bg(warning.opacity(0.28)))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .text_xs()
                    .font_semibold()
                    .text_color(warning)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(pane) = jump_pane {
                            this.reveal_pane(pane, window, cx);
                        }
                    }))
                    .child("去回复")
                    .into_any_element(),
            )
        } else if live_pane.is_none() {
            let dispatch_panel = cx.entity().downgrade();
            let dispatch_item = item.clone();
            let dispatch_panes = live_panes.to_vec();
            let dispatch_default = default_agent.map(str::to_string);
            Some(
                div()
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(format!("todo-rebind-{id}"))
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(theme.sidebar)
                                    .foreground(fg)
                                    .hover(theme.accent.opacity(0.6)),
                            )
                            .compact()
                            .label("派发")
                            .icon(IconName::ChevronDown)
                            .dropdown_menu_with_anchor(
                                Anchor::BottomRight,
                                move |menu, window, menu_cx| {
                                    Self::populate_dispatch_menu(
                                        menu,
                                        window,
                                        menu_cx,
                                        dispatch_panel.clone(),
                                        dispatch_item.clone(),
                                        dispatch_panes.clone(),
                                        dispatch_default.clone(),
                                    )
                                },
                            ),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

        let body = v_flex()
            .w_full()
            .py_2()
            .pr_2p5()
            .pl(if accent.is_some() { px(9.) } else { px(10.) })
            .gap_2()
            .when_some(accent, |el, color| el.border_l(px(3.)).border_color(color))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(item.title.clone()),
                    )
                    .child(more_btn),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_center()
                    .child(pill)
                    .child(div().flex_1())
                    .children(right_cta),
            );

        div()
            .id(format!("todo-card-ip-{id}"))
            .w_full()
            .rounded(radius)
            .bg(bg)
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .cursor_pointer()
            .hover(move |style| style.border_color(theme.link.opacity(0.45)))
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(pane) = jump_pane {
                    this.reveal_pane(pane, window, cx);
                } else {
                    this.open_detail(&card_item, window, cx);
                }
            }))
            .on_drag(
                DraggedTodo {
                    id,
                    title: drag_title,
                },
                |dragged, _, _, cx| {
                    cx.new(|_| DragPreview {
                        title: dragged.title.clone(),
                    })
                },
            )
            .child(body)
            .context_menu(move |menu, window, menu_cx| {
                Self::build_card_context_menu(
                    menu,
                    window,
                    menu_cx,
                    panel_weak.clone(),
                    menu_item.clone(),
                    jump_pane,
                    menu_panes.clone(),
                    menu_default.clone(),
                )
            })
            .into_any_element()
    }

    /// Pending Todo card with 1-click split dispatch (`▶ 交给 <DefaultAgent> | ▾`).
    pub(super) fn render_todo_card(
        &self,
        item: &TodoItem,
        live_panes: &[AgentPaneInfo],
        default_agent: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, fg, muted, radius) = (
            theme.background,
            theme.border,
            theme.foreground,
            theme.muted_foreground,
            theme.radius,
        );
        let id = item.id;
        let drag_title = item.title.clone();
        let open_item = item.clone();
        let more_item = item.clone();
        let dispatch_default_item = item.clone();
        let dropdown_item = item.clone();
        let dropdown_panes = live_panes.to_vec();
        let dropdown_default = default_agent.map(str::to_string);
        let dropdown_panel = cx.entity().downgrade();
        let ctx_panel = cx.entity().downgrade();
        let ctx_item = item.clone();
        let ctx_panes = live_panes.to_vec();
        let ctx_default = default_agent.map(str::to_string);

        let desc_preview = item
            .description
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string);

        let default_label = default_agent
            .map(short_agent_name)
            .unwrap_or_else(|| "Agent".to_string());
        let split_hover = theme.accent.opacity(0.75);

        let split_btn = h_flex()
            .flex_none()
            .h(px(22.))
            .rounded(px(5.))
            .bg(theme.sidebar)
            .overflow_hidden()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .id(format!("todo-quick-dispatch-{id}"))
                    .h_full()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .hover(move |style| style.bg(split_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.dispatch_default_agent(&dispatch_default_item, cx);
                    }))
                    .child(
                        Icon::new(IconName::Play)
                            .with_size(px(10.))
                            .text_color(theme.link),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(format!("交给 {default_label}")),
                    ),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(theme.sidebar_border))
            .child(
                Button::new(format!("todo-dispatch-menu-{id}"))
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(theme.transparent)
                            .foreground(muted)
                            .hover(split_hover),
                    )
                    .compact()
                    .icon(IconName::ChevronDown)
                    .tooltip("选择 Agent 或关联已有窗格")
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, window, menu_cx| {
                        Self::populate_dispatch_menu(
                            menu,
                            window,
                            menu_cx,
                            dropdown_panel.clone(),
                            dropdown_item.clone(),
                            dropdown_panes.clone(),
                            dropdown_default.clone(),
                        )
                    }),
            );

        let quick_done = div()
            .id(format!("todo-quick-done-{id}"))
            .flex_none()
            .p_1()
            .rounded(px(5.))
            .cursor_pointer()
            .text_color(muted)
            .hover(move |style| {
                style
                    .bg(theme.success.opacity(0.16))
                    .text_color(theme.success)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_status(id, TodoStatus::Done, cx);
            }))
            .child(Icon::new(IconName::Check).with_size(px(13.)));

        v_flex()
            .id(format!("todo-card-pending-{id}"))
            .w_full()
            .px_2p5()
            .py_2()
            .gap_1p5()
            .rounded(radius)
            .bg(bg)
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .hover(move |style| style.border_color(theme.link.opacity(0.45)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_detail(&open_item, window, cx);
            }))
            .on_drag(
                DraggedTodo {
                    id,
                    title: drag_title,
                },
                |dragged, _, _, cx| {
                    cx.new(|_| DragPreview {
                        title: dragged.title.clone(),
                    })
                },
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(item.title.clone()),
                    )
                    .child(
                        div()
                            .id(format!("todo-pending-more-{id}"))
                            .flex_none()
                            .p_0p5()
                            .rounded(radius)
                            .cursor_pointer()
                            .text_color(muted)
                            .hover(move |style| style.bg(theme.accent.opacity(0.6)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_detail(&more_item, window, cx);
                            }))
                            .child(Icon::new(IconName::Ellipsis).with_size(px(13.))),
                    ),
            )
            .when_some(desc_preview, |card, preview| {
                card.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(preview),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .pt_0p5()
                    .gap_1p5()
                    .items_center()
                    .child(split_btn)
                    .child(div().flex_1())
                    .child(quick_done),
            )
            .context_menu(move |menu, window, menu_cx| {
                Self::build_card_context_menu(
                    menu,
                    window,
                    menu_cx,
                    ctx_panel.clone(),
                    ctx_item.clone(),
                    None,
                    ctx_panes.clone(),
                    ctx_default.clone(),
                )
            })
            .into_any_element()
    }

    /// Lightweight flat row for completed items (matches sidebar density).
    pub(super) fn render_done_row(&self, item: &TodoItem, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, radius) = (
            theme.muted_foreground,
            theme.sidebar_accent.opacity(0.5),
            theme.radius,
        );
        let id = item.id;
        let open_item = item.clone();
        let drag_title = item.title.clone();
        let panel_weak = cx.entity().downgrade();
        let ctx_item = item.clone();

        h_flex()
            .id(format!("todo-done-row-{id}"))
            .w_full()
            .h(px(28.))
            .px_1p5()
            .gap_1p5()
            .rounded(radius)
            .items_center()
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_detail(&open_item, window, cx);
            }))
            .on_drag(
                DraggedTodo {
                    id,
                    title: drag_title,
                },
                |dragged, _, _, cx| {
                    cx.new(|_| DragPreview {
                        title: dragged.title.clone(),
                    })
                },
            )
            .child(
                Icon::new(IconName::Check)
                    .with_size(px(12.))
                    .text_color(theme.success),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .line_through()
                    .child(item.title.clone()),
            )
            .when_some(item.agent_id.as_deref(), |row, agent| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(short_agent_name(agent)),
                )
            })
            .context_menu(move |menu, window, menu_cx| {
                Self::build_card_context_menu(
                    menu,
                    window,
                    menu_cx,
                    panel_weak.clone(),
                    ctx_item.clone(),
                    None,
                    Vec::new(),
                    None,
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn build_card_context_menu(
        mut menu: gpui_kit::component::menu::PopupMenu,
        window: &mut Window,
        menu_cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
        panel: WeakEntity<Self>,
        item: TodoItem,
        jump_pane: Option<u64>,
        live_panes: Vec<AgentPaneInfo>,
        default_agent: Option<String>,
    ) -> gpui_kit::component::menu::PopupMenu {
        let id = item.id;
        if let Some(pane) = jump_pane {
            let p = panel.clone();
            menu = menu.item(
                PopupMenuItem::new("跳转到 Agent 窗格")
                    .icon(IconName::ExternalLink)
                    .on_click(move |_, window, cx| {
                        let _ = p.update(cx, |this, cx| this.reveal_pane(pane, window, cx));
                    }),
            );
        }
        {
            let p = panel.clone();
            let detail_item = item.clone();
            menu = menu.item(
                PopupMenuItem::new("查看与编辑").on_click(move |_, window, cx| {
                    let _ = p.update(cx, |this, cx| this.open_detail(&detail_item, window, cx));
                }),
            );
        }
        if item.status != TodoStatus::Done {
            let dp = panel.clone();
            let di = item.clone();
            menu = menu.submenu(
                "派发 / 关联 Agent",
                window,
                menu_cx,
                move |sub, win, scx| {
                    Self::populate_dispatch_menu(
                        sub,
                        win,
                        scx,
                        dp.clone(),
                        di.clone(),
                        live_panes.clone(),
                        default_agent.clone(),
                    )
                },
            );
        }
        menu = menu.separator();
        for status in [TodoStatus::Todo, TodoStatus::InProgress, TodoStatus::Done] {
            if item.status == status {
                continue;
            }
            let p = panel.clone();
            let label = match status {
                TodoStatus::Todo => "移至待办",
                TodoStatus::InProgress => "移至进行中",
                TodoStatus::Done => "标记为已完成",
            };
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                let _ = p.update(cx, |this, cx| this.set_status(id, status, cx));
            }));
        }
        if item.pane_id.is_some() {
            let p = panel.clone();
            menu = menu.item(
                PopupMenuItem::new("解除 Agent 关联").on_click(move |_, _, cx| {
                    let _ = p.update(cx, |this, cx| this.unbind_agent(id, cx));
                }),
            );
        }
        let p = panel.clone();
        menu.separator().item(
            PopupMenuItem::new("删除任务")
                .icon(IconName::Delete)
                .on_click(move |_, _, cx| {
                    let _ = p.update(cx, |this, cx| this.delete_item(id, cx));
                }),
        )
    }
}
