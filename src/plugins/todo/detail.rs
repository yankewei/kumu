use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{TodoPanel, short_agent_name};
use crate::agent::AgentState;
use crate::db::{TodoItem, TodoPatch, TodoStatus};
use crate::plugins::AgentPaneInfo;

/// Panel-internal detail view state (`← 返回看板`), keeping the center
/// terminals 100% visible while editing prompts or sending follow-ups.
pub(super) struct DetailEditor {
    pub(super) todo_id: i64,
    pub(super) title: Entity<InputState>,
    pub(super) desc: Entity<TextareaState>,
    pub(super) followup: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl DetailEditor {
    pub(super) fn new(item: &TodoItem, window: &mut Window, cx: &mut Context<TodoPanel>) -> Self {
        let todo_id = item.id;
        let title = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(item.title.clone())
                .placeholder("任务标题")
        });
        let desc = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("任务说明（派发时作为 Prompt 发送给 Agent）…")
                .auto_grow(3, 8)
        });
        if !item.description.is_empty() {
            let text = item.description.clone();
            desc.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        let followup = cx.new(|cx| InputState::new(window, cx).placeholder("补充修改意见，回车发送…"));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&title, move |panel, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let value = input.read(cx).value().trim().to_string();
                if !value.is_empty()
                    && let Some(workspace) = panel.host.workspace()
                {
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            todo_id,
                            &TodoPatch {
                                title: Some(value),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
            }
        }));
        subscriptions.push(cx.subscribe(&desc, move |panel, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let value = input.read(cx).value().to_string();
                if let Some(workspace) = panel.host.workspace() {
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            todo_id,
                            &TodoPatch {
                                description: Some(value),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
            }
        }));
        subscriptions.push(cx.subscribe_in(
            &followup,
            window,
            move |panel, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    panel.send_detail_followup(todo_id, window, cx);
                }
            },
        ));

        Self {
            todo_id,
            title,
            desc,
            followup,
            _subscriptions: subscriptions,
        }
    }
}

impl TodoPanel {
    fn send_detail_followup(&mut self, todo_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let text = detail.followup.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let pane_id = self
            .host
            .workspace()
            .and_then(|ws| ws.read(cx).todo(todo_id))
            .and_then(|item| item.pane_id);
        let Some(pane_id) = pane_id else {
            self.set_notice("该任务尚未关联运行中的 Agent 窗格", true);
            cx.notify();
            return;
        };
        match self.host.send_to_any_pane(pane_id, &text, cx) {
            Ok(_) => {
                if let Some(detail) = self.detail.as_ref() {
                    detail
                        .followup
                        .update(cx, |state, cx| state.set_value("", window, cx));
                }
                if let Some(workspace) = self.host.workspace() {
                    workspace.update(cx, |ws, cx| {
                        ws.update_todo(
                            todo_id,
                            &TodoPatch {
                                status: Some(TodoStatus::InProgress),
                                awaiting_review: Some(false),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
                self.notice = None;
            }
            Err(error) => {
                self.set_notice(error, true);
            }
        }
        cx.notify();
    }

    pub(super) fn render_detail(
        &self,
        item: &TodoItem,
        live_panes: &[AgentPaneInfo],
        pane_map: &HashMap<u64, AgentPaneInfo>,
        _window: &mut Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(detail) = self.detail.as_ref() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (bg, border, sidebar_border, fg, muted, radius) = (
            theme.background,
            theme.border,
            theme.sidebar_border,
            theme.foreground,
            theme.muted_foreground,
            theme.radius,
        );
        let id = item.id;
        let live_pane = item.pane_id.and_then(|pane| pane_map.get(&pane));
        let live_state = live_pane.and_then(|pane| pane.state);
        let needs_review = item.awaiting_review || live_state == Some(AgentState::Completed);

        let header = h_flex()
            .flex_none()
            .h(px(32.))
            .px_2()
            .gap_1p5()
            .items_center()
            .border_b_1()
            .border_color(sidebar_border)
            .child(
                Button::new("todo-detail-back")
                    .compact()
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .label("返回看板")
                    .on_click(cx.listener(|this, _, _, cx| this.close_detail(cx))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .text_color(muted)
                    .child(format!("#{id}")),
            )
            .child(
                Button::new("todo-detail-delete")
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(theme.transparent)
                            .foreground(muted)
                            .hover(theme.danger.opacity(0.14)),
                    )
                    .compact()
                    .icon(IconName::Delete)
                    .tooltip("删除任务")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.delete_item(id, cx);
                    })),
            );

        let status_seg = h_flex()
            .w_full()
            .p_0p5()
            .gap_0p5()
            .rounded(radius)
            .bg(bg)
            .border_1()
            .border_color(border)
            .children(
                [TodoStatus::Todo, TodoStatus::InProgress, TodoStatus::Done]
                    .into_iter()
                    .map(|status| {
                        let active = item.status == status;
                        div()
                            .id(format!("todo-detail-status-{}", status.as_str()))
                            .flex_1()
                            .h(px(24.))
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .when(active, |el| {
                                el.bg(theme.sidebar_accent)
                                    .text_color(theme.sidebar_accent_foreground)
                                    .font_semibold()
                            })
                            .when(!active, |el| {
                                el.text_color(muted)
                                    .hover(move |style| style.bg(theme.sidebar_accent.opacity(0.4)))
                            })
                            .text_xs()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_status(id, status, cx);
                            }))
                            .child(status.label())
                    }),
            );

        let agent_section: AnyElement = if let Some(pane) = live_pane {
            let (pill_bg, pill_fg, pill_label) = match live_state {
                Some(AgentState::Waiting) => (
                    theme.warning.opacity(0.16),
                    theme.warning,
                    format!("{} · 等待输入", short_agent_name(&pane.agent)),
                ),
                Some(AgentState::Completed) => (
                    theme.success.opacity(0.16),
                    theme.success,
                    format!("{} · 待确认", short_agent_name(&pane.agent)),
                ),
                _ if needs_review => (
                    theme.success.opacity(0.16),
                    theme.success,
                    format!("{} · 待确认", short_agent_name(&pane.agent)),
                ),
                Some(AgentState::Failed) => (
                    theme.danger.opacity(0.16),
                    theme.danger,
                    format!("{} · 执行失败", short_agent_name(&pane.agent)),
                ),
                Some(AgentState::Running) => (
                    theme.link.opacity(0.14),
                    theme.link,
                    format!("{} · 运行中", short_agent_name(&pane.agent)),
                ),
                None => (
                    theme.sidebar_accent.opacity(0.7),
                    muted,
                    format!("{} · 空闲", short_agent_name(&pane.agent)),
                ),
            };
            let pane_id = pane.pane;
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    v_flex()
                        .w_full()
                        .p_2p5()
                        .gap_2()
                        .rounded(radius)
                        .bg(bg)
                        .border_1()
                        .border_color(border)
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    h_flex()
                                        .h(px(20.))
                                        .px_1p5()
                                        .gap_1()
                                        .rounded(px(5.))
                                        .bg(pill_bg)
                                        .items_center()
                                        .child(
                                            div()
                                                .size(px(6.))
                                                .rounded_full()
                                                .bg(pill_fg),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_medium()
                                                .text_color(pill_fg)
                                                .child(pill_label),
                                        ),
                                )
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("窗格 {}", pane.ordinal)),
                                ),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1p5()
                                .child(
                                    Button::new("todo-detail-jump")
                                        .small()
                                        .ghost()
                                        .flex_1()
                                        .icon(IconName::ExternalLink)
                                        .label("跳转窗格")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.reveal_pane(pane_id, window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("todo-detail-unbind")
                                        .small()
                                        .ghost()
                                        .label("解绑")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.unbind_agent(id, cx);
                                        })),
                                ),
                        ),
                )
                .child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .text_color(muted)
                                .child("快速追问（直接发给该 Agent）"),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Input::new(&detail.followup).small()),
                                )
                                .child(
                                    Button::new("todo-detail-send-followup")
                                        .custom(
                                            ButtonCustomVariant::new(cx)
                                                .color(bg)
                                                .foreground(fg)
                                                .hover(theme.accent.opacity(0.6)),
                                        )
                                        .compact()
                                        .border_1()
                                        .border_color(border)
                                        .icon(IconName::ArrowRight)
                                        .tooltip("发送追问")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.send_detail_followup(id, window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element()
        } else {
            let default_agent = self.active_default_agent();
            let default_label = default_agent
                .as_deref()
                .map(short_agent_name)
                .unwrap_or_else(|| "Agent".to_string());
            let run_item = item.clone();
            let menu_item = item.clone();
            let menu_panes = live_panes.to_vec();
            let menu_default = default_agent.clone();
            let menu_panel = cx.entity().downgrade();
            v_flex()
                .w_full()
                .p_2p5()
                .gap_2()
                .rounded(radius)
                .bg(bg)
                .border_1()
                .border_color(border)
                .when_some(item.agent_id.as_deref(), |box_el, prev| {
                    box_el.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("上次关联：{}（窗格已关闭）", short_agent_name(prev))),
                    )
                })
                .child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .child(
                            Button::new("todo-detail-dispatch-default")
                                .small()
                                .primary()
                                .flex_1()
                                .icon(IconName::Play)
                                .label(format!("交给 {default_label}"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dispatch_default_agent(&run_item, cx);
                                })),
                        )
                        .child(
                            Button::new("todo-detail-dispatch-more")
                                .small()
                                .ghost()
                                .icon(IconName::ChevronDown)
                                .tooltip("更多派发或关联方式")
                                .dropdown_menu_with_anchor(
                                    Anchor::BottomRight,
                                    move |menu, window, menu_cx| {
                                        Self::populate_dispatch_menu(
                                            menu,
                                            window,
                                            menu_cx,
                                            menu_panel.clone(),
                                            menu_item.clone(),
                                            menu_panes.clone(),
                                            menu_default.clone(),
                                        )
                                    },
                                ),
                        ),
                )
                .into_any_element()
        };

        let footer_cta: AnyElement = if item.status == TodoStatus::Done {
            Button::new("todo-detail-reopen")
                .small()
                .ghost()
                .w_full()
                .label("重新打开（移回待办）")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_status(id, TodoStatus::Todo, cx);
                }))
                .into_any_element()
        } else if needs_review {
            let success = theme.success;
            Button::new("todo-detail-confirm-done")
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(success.opacity(0.16))
                        .foreground(success)
                        .hover(success.opacity(0.26)),
                )
                .small()
                .w_full()
                .icon(IconName::Check)
                .label("确认完成并归档")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_status(id, TodoStatus::Done, cx);
                    this.close_detail(cx);
                }))
                .into_any_element()
        } else {
            Button::new("todo-detail-mark-done")
                .small()
                .ghost()
                .w_full()
                .icon(IconName::Check)
                .label("标记为已完成")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_status(id, TodoStatus::Done, cx);
                    this.close_detail(cx);
                }))
                .into_any_element()
        };

        let body = v_flex()
            .id("todo-detail-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .when_some(self.notice.clone(), |el, (message, is_error)| {
                let color = if is_error { theme.danger } else { muted };
                el.child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(radius)
                        .bg(color.opacity(0.12))
                        .text_xs()
                        .text_color(color)
                        .child(message),
                )
            })
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(div().text_xs().font_medium().text_color(muted).child("标题"))
                    .child(Input::new(&detail.title).small()),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(div().text_xs().font_medium().text_color(muted).child("状态"))
                    .child(status_seg),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(muted)
                            .child("任务说明 / Agent Prompt"),
                    )
                    .child(
                        div()
                            .w_full()
                            .p_2()
                            .rounded(radius)
                            .bg(bg)
                            .border_1()
                            .border_color(border)
                            .child(
                                Textarea::new(&detail.desc)
                                    .appearance(false)
                                    .bordered(false),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(muted)
                            .child("关联 Agent"),
                    )
                    .child(agent_section),
            );

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .text_color(theme.sidebar_foreground)
            .child(header)
            .child(body)
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .child(footer_cta),
            )
            .into_any_element()
    }
}
