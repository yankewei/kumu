//! Pure Rust NSH UI for the left-hand Sidebar (projects, worktrees, live
//! global Agent status section, update notice, and settings entry).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::component::Theme;
use gpui_kit::{AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use crate::agent::AgentState;
use crate::app::layout::PaneId;
use crate::nsh_ui::theme::{appearance, nsh_theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProjectRowSnapshot {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) path: String,
    pub(super) worktree: bool,
    pub(super) active: bool,
    pub(super) unread: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentRowSnapshot {
    pub(super) pane: PaneId,
    pub(super) project_name: String,
    pub(super) agent: String,
    pub(super) title: String,
    pub(super) state: Option<AgentState>,
    pub(super) focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UpdateNoticeSnapshot {
    pub(super) icon: &'static str,
    pub(super) label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SidebarSnapshot {
    pub(super) projects: Vec<ProjectRowSnapshot>,
    pub(super) agents: Vec<AgentRowSnapshot>,
    pub(super) agents_collapsed: bool,
    pub(super) update_notice: Option<UpdateNoticeSnapshot>,
}

#[derive(Clone, Debug)]
pub(super) enum SidebarHostEvent {
    Snapshot(SidebarSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarAction {
    AddProject,
    SelectProject(i64),
    NewTerminal(i64),
    PromptCreateWorktree(i64),
    PromptOpenWorktree(i64),
    PromptRenameProject(i64),
    ConfirmDeleteProject(i64),
    RevealInFinder(String),
    CopyText(String),
    ToggleAgentsCollapsed,
    RevealAgent(PaneId),
    OpenSettings(bool),
}

struct SidebarMount {
    initial: SidebarSnapshot,
    events_rx: UnboundedReceiver<SidebarHostEvent>,
    actions_tx: UnboundedSender<SidebarAction>,
}

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, SidebarMount>>> = OnceLock::new();

fn pending_mounts() -> &'static Mutex<HashMap<u64, SidebarMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_mount(mount: SidebarMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

fn take_mount(id: u64) -> Option<SidebarMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

mod view {
    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::theme as active_theme;
    use nsh_core::ContextMenuEvent;
    use nsh_core::element::desktop::{MenuItem, popup_menu_at};
    use nsh_core::element::{AccessibilityRole, scroll};
    use nsh_core::prelude::*;

    use super::{
        AgentRowSnapshot, ProjectRowSnapshot, SidebarAction, SidebarHostEvent, SidebarSnapshot,
        take_mount,
    };
    use crate::agent::AgentState;
    use crate::nsh_ui::assets::icon_svg;
    use crate::nsh_ui::components::meta_text;
    use crate::nsh_ui::theme::{
        TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_xs_size,
    };

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct SidebarRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<SidebarAction>>, action: SidebarAction) {
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

    fn agent_state_color(state: Option<AgentState>) -> AdaptiveColor {
        match state {
            Some(AgentState::Running) => link_color(),
            Some(AgentState::Waiting) => warning_color(),
            Some(AgentState::Completed) => success_color(),
            Some(AgentState::Failed) => active_theme().destructive,
            None => active_theme().muted_foreground,
        }
    }

    fn agent_status_glyph(state: Option<AgentState>) -> AnyElement {
        let color = move || agent_state_color(state);
        let glyph: AnyElement = match state {
            Some(AgentState::Running) => icon_svg("icons/loader-circle.svg", 12.0)
                .text_color(color)
                .into_element(),
            Some(AgentState::Waiting) => view()
                .w(px(7.0))
                .h(px(7.0))
                .rounded(px(3.5))
                .bg(color)
                .into_element(),
            Some(AgentState::Completed) => icon_svg("icons/check.svg", 12.0)
                .text_color(color)
                .into_element(),
            Some(AgentState::Failed) => icon_svg("icons/triangle-alert.svg", 12.0)
                .text_color(color)
                .into_element(),
            None => view()
                .w(px(7.0))
                .h(px(7.0))
                .rounded(px(3.5))
                .border(px(1.5))
                .border_color(color)
                .into_element(),
        };
        view()
            .w(px(12.0))
            .h(px(12.0))
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .child(glyph)
            .into_element()
    }

    fn render_agent_summary(rows: &[AgentRowSnapshot]) -> AnyElement {
        let (mut waiting, mut failed, mut running, mut completed) =
            (0usize, 0usize, 0usize, 0usize);
        for row in rows {
            match row.state {
                Some(AgentState::Waiting) => waiting += 1,
                Some(AgentState::Failed) => failed += 1,
                Some(AgentState::Running) => running += 1,
                Some(AgentState::Completed) => completed += 1,
                None => {}
            }
        }
        let parts: Vec<(String, u8)> = [
            (waiting, "等待", 0u8),
            (failed, "失败", 1u8),
            (running, "运行中", 2u8),
            (completed, "已完成", 3u8),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, label, kind)| (format!("{count} {label}"), kind))
        .collect();

        if parts.is_empty() {
            return text(format!("{} 空闲", rows.len()))
                .flex_shrink_0()
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().muted_foreground)
                .into_element();
        }

        let mut line = view().flex_row().min_w_0().gap_1().items_center();
        for (idx, (label, kind)) in parts.into_iter().enumerate() {
            if idx > 0 {
                line = line.child(meta_text("·"));
            }
            line = line.child(
                text(label)
                    .flex_shrink_0()
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || match kind {
                        0 => warning_color(),
                        1 => active_theme().destructive,
                        2 => link_color(),
                        _ => success_color(),
                    }),
            );
        }
        line.into_element()
    }

    fn project_context_menu(
        tx: Option<UnboundedSender<SidebarAction>>,
        project: &ProjectRowSnapshot,
    ) -> Vec<MenuItem> {
        let project_id = project.id;
        let path = project.path.clone();
        let name = project.name.clone();
        let worktree = project.worktree;

        let mut items = Vec::new();
        {
            let tx = tx.clone();
            items.push(MenuItem::new("新建终端").on_select(move || {
                send(&tx, SidebarAction::NewTerminal(project_id));
            }));
        }
        if !worktree {
            let tx_c = tx.clone();
            let tx_o = tx.clone();
            items.push(MenuItem::new("新建 worktree").on_select(move || {
                send(&tx_c, SidebarAction::PromptCreateWorktree(project_id));
            }));
            items.push(MenuItem::new("打开已有 worktree").on_select(move || {
                send(&tx_o, SidebarAction::PromptOpenWorktree(project_id));
            }));
        }
        {
            let tx = tx.clone();
            let path = path.clone();
            items.push(MenuItem::new("在 Finder 中打开").on_select(move || {
                send(&tx, SidebarAction::RevealInFinder(path.clone()));
            }));
        }
        {
            let tx_p = tx.clone();
            let tx_n = tx.clone();
            items.push(MenuItem::submenu(
                "复制",
                [
                    MenuItem::new("复制路径").on_select(move || {
                        send(&tx_p, SidebarAction::CopyText(path.clone()));
                    }),
                    MenuItem::new("复制名称").on_select(move || {
                        send(&tx_n, SidebarAction::CopyText(name.clone()));
                    }),
                ],
            ));
        }
        items.push(MenuItem::separator());
        {
            let tx = tx.clone();
            items.push(MenuItem::new("重命名").on_select(move || {
                send(&tx, SidebarAction::PromptRenameProject(project_id));
            }));
        }
        items.push(MenuItem::separator());
        {
            let tx = tx.clone();
            items.push(
                MenuItem::new(if worktree { "移除" } else { "删除" })
                    .destructive(true)
                    .on_select(move || {
                        send(&tx, SidebarAction::ConfirmDeleteProject(project_id));
                    }),
            );
        }
        items
    }

    #[component]
    pub(super) fn SidebarRoot(props: SidebarRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (SidebarSnapshot::default(), None, None),
        };

        let state = signal(initial);
        let hovered_project = signal(Option::<i64>::None);

        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        SidebarHostEvent::Snapshot(next) => state.set(next),
                    }
                }
            });
        }

        let tx_add = actions_tx.clone();
        let header = view()
            .flex_row()
            .flex_shrink_0()
            .px_3()
            .py_1p5()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_b_color(move || active_theme().sidebar_border)
            .child(
                text("项目")
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || active_theme().muted_foreground),
            )
            .child(
                view()
                    .role(AccessibilityRole::Button)
                    .label("打开项目")
                    .rounded(move || active_theme().radius)
                    .p_0p5()
                    .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
                    .on_press(move || send(&tx_add, SidebarAction::AddProject))
                    .child(
                        icon_svg("icons/plus.svg", 14.0)
                            .text_color(move || active_theme().muted_foreground),
                    ),
            );

        let tx_body = actions_tx.clone();
        let middle = dynamic(move || {
            let snap = state.get();

            // Project tree
            let mut proj_col = view().flex_col().w_full().pb_2();
            for project in &snap.projects {
                let id = project.id;
                let active = project.active;
                let worktree = project.worktree;
                let tx_sel = tx_body.clone();
                let tx_del = tx_body.clone();
                let tx_term = tx_body.clone();
                let tx_ctx = tx_body.clone();
                let ctx_proj = project.clone();

                let fg = move || {
                    if active {
                        active_theme().sidebar_accent_foreground
                    } else {
                        active_theme().sidebar_foreground
                    }
                };

                let icon_path = if worktree {
                    "icons/git-branch.svg"
                } else if active {
                    "icons/folder-open.svg"
                } else {
                    "icons/folder.svg"
                };

                let mut row = view()
                    .role(AccessibilityRole::Button)
                    .flex_row()
                    .w_full()
                    .py_1p5()
                    .pr_3()
                    .pl(px(if worktree { 28.0 } else { 12.0 }))
                    .mt(px(if worktree { 2.0 } else { 6.0 }))
                    .gap_1p5()
                    .items_center()
                    .on_mouse_enter(move |_: &nsh_core::MouseEvent| hovered_project.set(Some(id)))
                    .on_mouse_leave(move |_: &nsh_core::MouseEvent| {
                        if hovered_project.get_untracked() == Some(id) {
                            hovered_project.set(None);
                        }
                    })
                    .on_press(move || send(&tx_sel, SidebarAction::SelectProject(id)))
                    .on_context_menu(move |ev: &ContextMenuEvent| {
                        let items = project_context_menu(tx_ctx.clone(), &ctx_proj);
                        drop(popup_menu_at(items, ev.page_position));
                    })
                    .child(icon_svg(icon_path, 14.0).text_color(fg))
                    .child(
                        text(project.name.clone())
                            .flex_1()
                            .min_w_0()
                            .text_size(text_xs_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg),
                    );

                if active {
                    row = row.bg(move || active_theme().sidebar_accent);
                } else {
                    row = row.hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)));
                }

                if project.unread {
                    row = row.child(
                        view()
                            .w(px(6.0))
                            .h(px(6.0))
                            .flex_shrink_0()
                            .rounded(px(3.0))
                            .bg(link_color),
                    );
                }

                let row_actions = when(
                    move || hovered_project.get() == Some(id),
                    move || {
                        let tx_del = tx_del.clone();
                        let tx_term = tx_term.clone();
                        view()
                            .flex_row()
                            .flex_shrink_0()
                            .gap_0p5()
                            .child(
                                view()
                                    .role(AccessibilityRole::Button)
                                    .label(if worktree {
                                        "移除 worktree"
                                    } else {
                                        "移除项目"
                                    })
                                    .rounded(move || active_theme().radius)
                                    .p_0p5()
                                    .hover(|s| {
                                        s.bg(move || active_theme().sidebar_accent.opacity(0.6))
                                    })
                                    .on_press(move || {
                                        send(&tx_del, SidebarAction::ConfirmDeleteProject(id));
                                    })
                                    .child(
                                        icon_svg("icons/x.svg", 13.0)
                                            .text_color(move || active_theme().muted_foreground),
                                    ),
                            )
                            .child(
                                view()
                                    .role(AccessibilityRole::Button)
                                    .label("新建终端")
                                    .rounded(move || active_theme().radius)
                                    .p_0p5()
                                    .hover(|s| {
                                        s.bg(move || active_theme().sidebar_accent.opacity(0.6))
                                    })
                                    .on_press(move || {
                                        send(&tx_term, SidebarAction::NewTerminal(id));
                                    })
                                    .child(
                                        icon_svg("icons/square-terminal.svg", 13.0)
                                            .text_color(move || active_theme().muted_foreground),
                                    ),
                            )
                    },
                );
                row = row.child(row_actions);
                proj_col = proj_col.child(row);
            }

            let project_list = scroll().flex_1().min_h_0().child(proj_col);

            // Global Agent status section (only shown when live agents exist)
            let mut container = view().flex_col().flex_1().min_h_0().child(project_list);
            if !snap.agents.is_empty() {
                let collapsed = snap.agents_collapsed;
                let tx_toggle = tx_body.clone();
                let mut hdr = view()
                    .role(AccessibilityRole::Button)
                    .flex_row()
                    .w_full()
                    .h(px(32.0))
                    .px_3()
                    .gap_1p5()
                    .flex_shrink_0()
                    .items_center()
                    .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
                    .on_press(move || send(&tx_toggle, SidebarAction::ToggleAgentsCollapsed))
                    .child(
                        text("Agent 状态")
                            .flex_shrink_0()
                            .text_size(text_xs_size)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .child(view().flex_1());

                if collapsed {
                    hdr = hdr.child(render_agent_summary(&snap.agents));
                } else {
                    hdr = hdr.child(
                        text(snap.agents.len().to_string())
                            .text_size(text_xs_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || active_theme().muted_foreground),
                    );
                }
                hdr = hdr.child(
                    icon_svg(
                        if collapsed {
                            "icons/chevron-right.svg"
                        } else {
                            "icons/chevron-down.svg"
                        },
                        12.0,
                    )
                    .text_color(move || active_theme().muted_foreground),
                );

                let mut section = view()
                    .flex_col()
                    .w_full()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_t_color(move || active_theme().sidebar_border)
                    .child(hdr);

                if !collapsed {
                    let mut rows_col = view().flex_col().w_full();
                    for row in &snap.agents {
                        let pane = row.pane;
                        let focused = row.focused;
                        let state = row.state;
                        let tx_reveal = tx_body.clone();
                        let title_color = move || {
                            if focused {
                                active_theme().sidebar_accent_foreground
                            } else if matches!(state, None | Some(AgentState::Completed)) {
                                active_theme().muted_foreground
                            } else {
                                active_theme().sidebar_foreground
                            }
                        };

                        let mut agent_el = view()
                            .role(AccessibilityRole::Button)
                            .flex_row()
                            .w_full()
                            .h(px(40.0))
                            .flex_shrink_0()
                            .px_3()
                            .gap_2()
                            .items_center()
                            .on_press(move || send(&tx_reveal, SidebarAction::RevealAgent(pane)))
                            .child(agent_status_glyph(state))
                            .child(
                                view()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        text(row.title.clone())
                                            .text_size(text_xs_size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(title_color),
                                    )
                                    .child(
                                        view()
                                            .flex_row()
                                            .w_full()
                                            .min_w_0()
                                            .gap_1()
                                            .items_center()
                                            .child(meta_text(row.agent.clone()))
                                            .child(meta_text("·"))
                                            .child(meta_text(row.project_name.clone())),
                                    ),
                            );
                        if focused {
                            agent_el = agent_el.bg(move || active_theme().sidebar_accent);
                        } else {
                            agent_el = agent_el.hover(|s| {
                                s.bg(move || active_theme().sidebar_accent.opacity(0.5))
                            });
                        }
                        rows_col = rows_col.child(agent_el);
                    }
                    section = section.child(scroll().w_full().max_h(px(240.0)).child(rows_col));
                }
                container = container.child(section);
            }

            container
        });

        let tx_footer = actions_tx.clone();
        let footer = dynamic(move || {
            let snap = state.get();
            let tx_upd = tx_footer.clone();
            let tx_set = tx_footer.clone();

            let mut col = view()
                .flex_col()
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .gap_1()
                .border_t_1()
                .border_t_color(move || active_theme().sidebar_border);

            if let Some(notice) = snap.update_notice {
                col = col.child(
                    view()
                        .role(AccessibilityRole::Button)
                        .flex_row()
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1p5()
                        .rounded(move || active_theme().radius)
                        .items_center()
                        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
                        .on_press(move || send(&tx_upd, SidebarAction::OpenSettings(true)))
                        .child(icon_svg(notice.icon, 13.0).text_color(link_color))
                        .child(
                            text(notice.label)
                                .text_size(text_xs_size)
                                .text_color(link_color),
                        ),
                );
            }

            col.child(
                view().flex_row().w_full().justify_between().child(
                    view()
                        .role(AccessibilityRole::Button)
                        .label("设置")
                        .rounded(move || active_theme().radius)
                        .p_1()
                        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
                        .on_press(move || send(&tx_set, SidebarAction::OpenSettings(false)))
                        .child(
                            icon_svg("icons/settings.svg", 14.0)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                ),
            )
        });

        view()
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
            .child(header)
            .child(middle)
            .child(footer)
    }
}

/// Host handle for the Pure Rust NSH sidebar owned by `KumuApp`.
pub(super) struct NshSidebar {
    mount_id: u64,
    pub(super) app_view: Entity<AppView>,
    events_tx: UnboundedSender<SidebarHostEvent>,
    last_snapshot: SidebarSnapshot,
    _actions_task: Task<()>,
}

impl Drop for NshSidebar {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl NshSidebar {
    pub(super) fn new<V: 'static>(
        initial: SidebarSnapshot,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_action: impl FnMut(&mut V, SidebarAction, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (events_tx, events_rx) = unbounded::<SidebarHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<SidebarAction>();
        let mount_id = register_mount(SidebarMount {
            initial: initial.clone(),
            events_rx,
            actions_tx,
        });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Sidebar".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(
                Root::new::<view::SidebarRoot>(move || view::SidebarRootProps {
                    mount_id: mount_id.into(),
                }),
            ));
            view
        });

        let actions_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let res = this.update_in(cx, |host, window, cx| {
                    on_action(host, action, window, cx);
                });
                if res.is_err() {
                    break;
                }
            }
        });

        Self {
            mount_id,
            app_view,
            events_tx,
            last_snapshot: initial,
            _actions_task: actions_task,
        }
    }

    pub(super) fn sync(&mut self, snapshot: SidebarSnapshot) {
        if self.last_snapshot != snapshot {
            self.last_snapshot = snapshot.clone();
            let _ = self
                .events_tx
                .unbounded_send(SidebarHostEvent::Snapshot(snapshot));
        }
    }

    pub(super) fn sync_theme(&self, theme: &Theme, cx: &mut gpui_kit::App) {
        let (mapped, mode) = (nsh_theme(theme), appearance(theme.mode));
        self.app_view.update(cx, |view, cx| {
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn sidebar_nsh_selects_projects_and_toggles_agents() {
        let (_events_tx, events_rx) = unbounded::<SidebarHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<SidebarAction>();
        let initial = SidebarSnapshot {
            projects: vec![ProjectRowSnapshot {
                id: 7,
                name: "kumu".into(),
                path: "/tmp/kumu".into(),
                worktree: false,
                active: true,
                unread: false,
            }],
            agents: vec![AgentRowSnapshot {
                pane: PaneId(3),
                project_name: "kumu".into(),
                agent: "claude".into(),
                title: "Refactor UI".into(),
                state: Some(AgentState::Waiting),
                focused: false,
            }],
            agents_collapsed: false,
            update_notice: None,
        };
        let mount_id = register_mount(SidebarMount {
            initial,
            events_rx,
            actions_tx,
        });
        let root =
            nsh_core::element::Root::new::<view::SidebarRoot>(move || view::SidebarRootProps {
                mount_id: mount_id.into(),
            });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start sidebar NSH");

        app.wait_for("Refactor UI").expect("agent row renders");
        app.press("Refactor UI").expect("click agent row");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(SidebarAction::RevealAgent(PaneId(3)))
        );
        app.press("Agent 状态").expect("toggle agents section");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(SidebarAction::ToggleAgentsCollapsed)
        );
    }
}
