//! Pure Rust NSH UI for the left-hand Sidebar (projects, worktrees, live
//! global Agent status section, update notice, and settings entry).

use crate::agent::AgentState;
use crate::app::layout::PaneId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProjectRowKind {
    /// Non-git directory: a standalone clickable project row.
    PlainProject,
    /// Git repository group header; its worktrees (starting with main) sit below.
    RepoHeader,
    /// Main checkout of a git repository (e.g. `main`), bound to the root project id.
    MainWorktree,
    /// Linked git worktree row, bound to its own project id.
    LinkedWorktree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProjectRowSnapshot {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) path: String,
    pub(super) kind: ProjectRowKind,
    /// Owning repository name for worktree rows (used by command palette search).
    pub(super) repo_name: Option<String>,
    pub(super) active: bool,
    /// True on a `RepoHeader` when its main or any linked worktree is selected.
    pub(super) has_active_child: bool,
    pub(super) unread: bool,
    /// Open TODO count for root `RepoHeader` / `PlainProject` rows.
    pub(super) todo_count: usize,
    /// True when any linked Agent on this board needs human attention.
    pub(super) todo_attention: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentRowSnapshot {
    pub(super) pane: PaneId,
    pub(super) project_name: String,
    pub(super) project_path: String,
    pub(super) tab_name: String,
    pub(super) agent: String,
    pub(super) title: String,
    pub(super) state: Option<AgentState>,
    pub(super) focused: bool,
    /// The agent changed status while the user was looking elsewhere.
    pub(super) unseen: bool,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarAction {
    AddProject,
    SelectProject(i64),
    OpenProjectBoard(i64),
    NewTerminal(i64),
    PromptCreateWorktree(i64),
    PromptOpenWorktree(i64),
    PromptRenameProject(i64),
    ConfirmDeleteProject(i64),
    ConfirmRemoveWorktree(i64),
    RevealInFinder(String),
    CopyText(String),
    ToggleAgentsCollapsed,
    RevealAgent(PaneId),
    OpenSettings(bool),
}

pub(super) mod view {
    use std::sync::Arc;

    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::theme as active_theme;
    use nsh_core::ContextMenuEvent;
    use nsh_core::element::desktop::{MenuItem, popup_menu_at};
    use nsh_core::element::{scroll, virtual_list};
    use nsh_core::prelude::*;

    const AGENT_ROW_HEIGHT: f32 = 40.0;

    use super::{
        AgentRowSnapshot, ProjectRowKind, ProjectRowSnapshot, SidebarAction, SidebarSnapshot,
    };
    use crate::agent::AgentState;
    use crate::nsh_ui::assets::icon_svg;
    use crate::nsh_ui::components::{
        ControlTone, kumu_icon_button, kumu_list_button, kumu_tooltip, meta_text,
    };
    use crate::nsh_ui::theme::{
        TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_xs_size,
    };

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
                .app_class("animate-spin")
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
        let unseen = rows.iter().any(|row| row.unseen);
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

        if rows.is_empty() {
            return text("0")
                .flex_shrink_0()
                .text_size(text_xs_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || active_theme().muted_foreground)
                .into_element();
        }

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
        if unseen {
            line = line.child(
                view()
                    .w(px(6.0))
                    .h(px(6.0))
                    .flex_shrink_0()
                    .rounded(px(3.0))
                    .bg(link_color),
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
        let kind = project.kind;

        let mut items = Vec::new();
        if matches!(
            kind,
            ProjectRowKind::PlainProject | ProjectRowKind::RepoHeader
        ) {
            let tx_board = tx.clone();
            items.push(MenuItem::new("打开 TODO 看板").on_select(move || {
                send(&tx_board, SidebarAction::OpenProjectBoard(project_id));
            }));
        }
        if kind != ProjectRowKind::RepoHeader {
            let tx = tx.clone();
            items.push(MenuItem::new("新建终端").on_select(move || {
                send(&tx, SidebarAction::NewTerminal(project_id));
            }));
        }
        if matches!(
            kind,
            ProjectRowKind::PlainProject | ProjectRowKind::RepoHeader
        ) {
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
            let copy_name_label = if kind == ProjectRowKind::MainWorktree {
                "复制分支名"
            } else {
                "复制名称"
            };
            items.push(MenuItem::submenu(
                "复制",
                [
                    MenuItem::new("复制路径").on_select(move || {
                        send(&tx_p, SidebarAction::CopyText(path.clone()));
                    }),
                    MenuItem::new(copy_name_label).on_select(move || {
                        send(&tx_n, SidebarAction::CopyText(name.clone()));
                    }),
                ],
            ));
        }
        if kind != ProjectRowKind::MainWorktree {
            items.push(MenuItem::separator());
            {
                let tx = tx.clone();
                items.push(MenuItem::new("重命名").on_select(move || {
                    send(&tx, SidebarAction::PromptRenameProject(project_id));
                }));
            }
            items.push(MenuItem::separator());
            if kind == ProjectRowKind::LinkedWorktree {
                {
                    let tx = tx.clone();
                    items.push(MenuItem::new("从列表移除").on_select(move || {
                        send(&tx, SidebarAction::ConfirmDeleteProject(project_id));
                    }));
                }
                {
                    let tx = tx.clone();
                    items.push(
                        MenuItem::new("删除 worktree（清理目录）")
                            .destructive(true)
                            .on_select(move || {
                                send(&tx, SidebarAction::ConfirmRemoveWorktree(project_id));
                            }),
                    );
                }
            } else {
                let tx = tx.clone();
                items.push(MenuItem::new("删除").destructive(true).on_select(move || {
                    send(&tx, SidebarAction::ConfirmDeleteProject(project_id));
                }));
            }
        }
        items
    }

    pub(in crate::ui::home) fn render_sidebar(
        state: Signal<SidebarSnapshot>,
        actions_tx: Option<UnboundedSender<SidebarAction>>,
    ) -> impl IntoElement {
        let hovered_row = signal(Option::<(i64, ProjectRowKind)>::None);

        let tx_body = actions_tx.clone();
        let middle = dynamic(move || {
            let snap = state.get();

            // Global Agent status section: the bottom 40% of the sidebar body,
            // always the same height whether its rows are shown or folded.
            // It sits below the projects; its body virtual-scrolls.
            let collapsed = snap.agents_collapsed;
            let tx_toggle = tx_body.clone();
            let mut hdr = kumu_list_button("切换 Agent 状态列表", false, ControlTone::Sidebar)
                .w_full()
                .h(px(32.0))
                .px_3()
                .gap_1p5()
                .flex_shrink_0()
                .items_center()
                .on_press(move || send(&tx_toggle, SidebarAction::ToggleAgentsCollapsed))
                .child(
                    text("Agent 状态")
                        .flex_shrink_0()
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || active_theme().muted_foreground),
                )
                .child(view().flex_1());

            if !collapsed {
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

            // Folding swaps the list for a centered summary; the body keeps
            // the section's height so neither action reflows the projects.
            let agent_body: AnyElement = if collapsed {
                let summary: AnyElement = if snap.agents.is_empty() {
                    meta_text("暂无运行中的 Agent").into_element()
                } else {
                    render_agent_summary(&snap.agents)
                };
                view()
                    .flex_col()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .items_center()
                    .justify_center()
                    .child(summary)
                    .into_element()
            } else if snap.agents.is_empty() {
                view()
                    .flex_col()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .px_3()
                    .child(meta_text("暂无运行中的 Agent"))
                    .into_element()
            } else {
                let rows = Arc::new(snap.agents);
                let row_count = rows.len();
                let tx_rows = tx_body.clone();
                virtual_list(row_count, move |pos| {
                    let Some(row) = rows.get(pos.index()) else {
                        return view().h(px(AGENT_ROW_HEIGHT)).into_element();
                    };
                    let pane = row.pane;
                    let focused = row.focused;
                    let state = row.state;
                    let unseen = row.unseen;
                    let tx_reveal = tx_rows.clone();
                    let title_color = move || {
                        if focused {
                            active_theme().sidebar_accent_foreground
                        } else if state.is_none()
                            || (state == Some(AgentState::Completed) && !unseen)
                        {
                            active_theme().muted_foreground
                        } else {
                            active_theme().sidebar_foreground
                        }
                    };
                    let hover_detail = if row.project_path.is_empty() {
                        format!("{}\n{} · {}", row.title, row.project_name, row.tab_name)
                    } else {
                        format!(
                            "{}\n{} · {}\n{}",
                            row.title, row.project_name, row.tab_name, row.project_path
                        )
                    };

                    let mut row_button =
                        kumu_list_button(hover_detail.clone(), focused, ControlTone::Sidebar)
                            .h(px(AGENT_ROW_HEIGHT))
                            .flex_shrink_0()
                            .px_3()
                            .gap_2()
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
                                            .w_full()
                                            .min_w_0()
                                            .app_class("truncate")
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
                                            .child(meta_text(row.agent.clone()).flex_shrink_0())
                                            .child(meta_text("·").flex_shrink_0())
                                            .child(
                                                meta_text(row.project_name.clone())
                                                    .flex_1()
                                                    .min_w_0()
                                                    .app_class("truncate"),
                                            ),
                                    ),
                            );
                    // Waiting and failed already read as urgent through their
                    // glyph; a finished turn needs this quiet dot until seen.
                    if unseen && state == Some(AgentState::Completed) {
                        row_button = row_button.child(
                            view()
                                .w(px(6.0))
                                .h(px(6.0))
                                .flex_shrink_0()
                                .rounded(px(3.0))
                                .bg(move || agent_state_color(state)),
                        );
                    }
                    let agent_el = view()
                        .tooltip(kumu_tooltip(hover_detail.clone()))
                        .child(row_button);
                    agent_el.into_element()
                })
                .estimated_item_size(px(AGENT_ROW_HEIGHT))
                .w_full()
                .flex_1()
                .min_h_0()
                .into_element()
            };

            let agent_section = view()
                .flex_col()
                .w_full()
                .flex_1()
                .flex_grow(2.0)
                .min_h_0()
                .border_t_1()
                .border_t_color(move || active_theme().sidebar_border)
                .child(hdr)
                .child(agent_body);

            // Project section: the top 60%, a fixed header with the add
            // button, then the scrolling project tree.
            let tx_add = tx_body.clone();
            let project_header = view()
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
                .child(kumu_icon_button(
                    "打开项目",
                    icon_svg("icons/plus.svg", 14.0)
                        .text_color(move || active_theme().muted_foreground),
                    ControlTone::Sidebar,
                    move || send(&tx_add, SidebarAction::AddProject),
                ));

            // Project tree
            let mut proj_col = view().flex_col().w_full().px_1p5().py_1p5().gap_0p5();
            for (idx, project) in snap.projects.iter().enumerate() {
                let id = project.id;
                let kind = project.kind;
                let row_key = (id, kind);
                let active = project.active;
                let has_active_child = project.has_active_child;
                let is_worktree = matches!(
                    kind,
                    ProjectRowKind::MainWorktree | ProjectRowKind::LinkedWorktree
                );
                let is_header = kind == ProjectRowKind::RepoHeader;
                let tx_sel = tx_body.clone();
                let tx_del = tx_body.clone();
                let tx_term = tx_body.clone();
                let tx_wt = tx_body.clone();
                let tx_ctx = tx_body.clone();
                let ctx_proj = project.clone();

                let text_fg = move || {
                    if active {
                        active_theme().sidebar_accent_foreground
                    } else if is_header {
                        if has_active_child {
                            active_theme().sidebar_foreground
                        } else {
                            active_theme().muted_foreground
                        }
                    } else {
                        active_theme().sidebar_foreground
                    }
                };
                let icon_fg = move || {
                    if active {
                        active_theme().sidebar_accent_foreground
                    } else {
                        active_theme().muted_foreground
                    }
                };

                let icon_path = if is_worktree {
                    "icons/git-branch.svg"
                } else if active || has_active_child {
                    "icons/folder-open.svg"
                } else {
                    "icons/folder.svg"
                };

                let project_path = project.path.clone();
                let top_margin = if is_worktree || idx == 0 {
                    0.0
                } else if is_header {
                    6.0
                } else {
                    4.0
                };
                let mut row = view()
                    .flex_row()
                    .w_full()
                    .h(px(if is_header || is_worktree { 28.0 } else { 30.0 }))
                    .pr_1()
                    .mt(px(top_margin))
                    .items_center()
                    .rounded(move || active_theme().radius)
                    .bg(move || {
                        if active {
                            active_theme().sidebar_accent
                        } else if hovered_row.get() == Some(row_key) {
                            active_theme().sidebar_accent.opacity(0.5)
                        } else {
                            AdaptiveColor::TRANSPARENT
                        }
                    })
                    .when(!reduce_motion(), |s| s.transition_colors().duration(120.0))
                    .on_mouse_enter(move |_: &nsh_core::MouseEvent| hovered_row.set(Some(row_key)))
                    .on_mouse_leave(move |_: &nsh_core::MouseEvent| {
                        if hovered_row.get_untracked() == Some(row_key) {
                            hovered_row.set(None);
                        }
                    });

                let mut project_button = nsh_base::button()
                    .accessibility_label(project.name.clone())
                    .selected(active)
                    .flex_row()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .items_center()
                    .rounded(move || active_theme().radius)
                    .active(|s| s.opacity(0.75))
                    .focus_visible(|s| {
                        s.outline(px(2.0))
                            .outline_color(move || active_theme().ring)
                    })
                    .pl(px(if is_worktree { 22.0 } else { 6.0 }))
                    .pr_1()
                    .gap_1p5()
                    .on_press(move || {
                        if is_header {
                            send(&tx_sel, SidebarAction::OpenProjectBoard(id));
                        } else {
                            send(&tx_sel, SidebarAction::SelectProject(id));
                        }
                    })
                    .on_context_menu(move |ev: &ContextMenuEvent| {
                        let items = project_context_menu(tx_ctx.clone(), &ctx_proj);
                        drop(popup_menu_at(items, ev.page_position));
                    })
                    .child(icon_svg(icon_path, 14.0).text_color(icon_fg))
                    .child(
                        text(project.name.clone())
                            .flex_1()
                            .min_w_0()
                            .app_class("truncate")
                            .tooltip(kumu_tooltip(project_path))
                            .text_size(text_xs_size)
                            .font_weight(if is_header {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(text_fg),
                    );

                if is_header && project.todo_attention {
                    project_button = project_button.child(
                        view()
                            .w(px(6.0))
                            .h(px(6.0))
                            .flex_shrink_0()
                            .rounded(px(3.0))
                            .bg(warning_color),
                    );
                }
                if is_header && project.todo_count > 0 {
                    let count_str = project.todo_count.to_string();
                    project_button = project_button.child(
                        view()
                            .px_1p5()
                            .py_0p5()
                            .flex_shrink_0()
                            .rounded(px(999.0))
                            .bg(move || {
                                if active {
                                    active_theme().background
                                } else {
                                    link_color().opacity(0.14)
                                }
                            })
                            .child(
                                text(count_str)
                                    .text_size(text_xs_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(link_color),
                            ),
                    );
                }

                if project.unread {
                    project_button = project_button.child(
                        view()
                            .w(px(6.0))
                            .h(px(6.0))
                            .flex_shrink_0()
                            .rounded(px(3.0))
                            .bg(link_color),
                    );
                }

                let row_actions = dynamic(move || {
                    if hovered_row.get() != Some(row_key) {
                        return view().into_element();
                    }
                    let tx_del = tx_del.clone();
                    let tx_term = tx_term.clone();
                    let tx_wt = tx_wt.clone();
                    let mut actions = view().flex_row().flex_shrink_0().items_center().gap_0p5();
                    match kind {
                        ProjectRowKind::RepoHeader => {
                            actions = actions
                                .child(kumu_icon_button(
                                    "移除项目",
                                    icon_svg("icons/x.svg", 13.0)
                                        .text_color(move || active_theme().muted_foreground),
                                    ControlTone::Sidebar,
                                    move || {
                                        send(&tx_del, SidebarAction::ConfirmDeleteProject(id));
                                    },
                                ))
                                .child(kumu_icon_button(
                                    "新建 worktree",
                                    icon_svg("icons/plus.svg", 13.0)
                                        .text_color(move || active_theme().muted_foreground),
                                    ControlTone::Sidebar,
                                    move || {
                                        send(&tx_wt, SidebarAction::PromptCreateWorktree(id));
                                    },
                                ));
                        }
                        ProjectRowKind::MainWorktree => {
                            actions = actions.child(kumu_icon_button(
                                "新建终端",
                                icon_svg("icons/square-terminal.svg", 13.0)
                                    .text_color(move || active_theme().muted_foreground),
                                ControlTone::Sidebar,
                                move || {
                                    send(&tx_term, SidebarAction::NewTerminal(id));
                                },
                            ));
                        }
                        ProjectRowKind::PlainProject | ProjectRowKind::LinkedWorktree => {
                            let remove_label = if kind == ProjectRowKind::LinkedWorktree {
                                "移除 worktree"
                            } else {
                                "移除项目"
                            };
                            actions = actions
                                .child(kumu_icon_button(
                                    remove_label,
                                    icon_svg("icons/x.svg", 13.0)
                                        .text_color(move || active_theme().muted_foreground),
                                    ControlTone::Sidebar,
                                    move || {
                                        send(&tx_del, SidebarAction::ConfirmDeleteProject(id));
                                    },
                                ))
                                .child(kumu_icon_button(
                                    "新建终端",
                                    icon_svg("icons/square-terminal.svg", 13.0)
                                        .text_color(move || active_theme().muted_foreground),
                                    ControlTone::Sidebar,
                                    move || {
                                        send(&tx_term, SidebarAction::NewTerminal(id));
                                    },
                                ));
                        }
                    }
                    actions.into_element()
                });
                row = row.child(project_button).child(row_actions);
                proj_col = proj_col.child(row);
            }

            let project_list = scroll().flex_1().min_h_0().child(proj_col);

            let project_section = view()
                .flex_col()
                .w_full()
                .flex_1()
                .flex_grow(3.0)
                .min_h_0()
                .child(project_header)
                .child(project_list);

            view()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(project_section)
                .child(agent_section)
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
                let mut notice_icon = icon_svg(notice.icon, 13.0).text_color(link_color);
                if notice.icon == "icons/loader-circle.svg" {
                    notice_icon = notice_icon.app_class("animate-spin");
                }
                col = col.child(
                    kumu_list_button(notice.label.clone(), false, ControlTone::Sidebar)
                        .px_2()
                        .py_1()
                        .gap_1p5()
                        .on_press(move || send(&tx_upd, SidebarAction::OpenSettings(true)))
                        .child(notice_icon)
                        .child(
                            text(notice.label)
                                .flex_1()
                                .min_w_0()
                                .app_class("truncate")
                                .text_size(text_xs_size)
                                .text_color(link_color),
                        ),
                );
            }

            col.child(
                view()
                    .flex_row()
                    .w_full()
                    .justify_between()
                    .child(kumu_icon_button(
                        "设置",
                        icon_svg("icons/settings.svg", 14.0)
                            .text_color(move || active_theme().muted_foreground),
                        ControlTone::Sidebar,
                        move || send(&tx_set, SidebarAction::OpenSettings(false)),
                    )),
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
            .child(middle)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use nsh_core::prelude::*;
    use nsh_engine::EngineConfig;

    use super::*;
    use crate::nsh_ui::mount::MountRegistry;

    struct SidebarMount {
        initial: SidebarSnapshot,
        events_rx: UnboundedReceiver<SidebarSnapshot>,
        actions_tx: UnboundedSender<SidebarAction>,
    }

    static MOUNTS: MountRegistry<SidebarMount> = MountRegistry::new();

    #[derive(Clone, Debug, PartialEq, Props)]
    struct SidebarRootProps {
        mount_id: Prop<u64>,
    }

    #[component]
    fn SidebarRoot(props: SidebarRootProps) -> impl IntoElement {
        let mount = MOUNTS.take(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (SidebarSnapshot::default(), None, None),
        };
        let state = signal(initial);
        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(next) = rx.next().await {
                    state.set(next);
                }
            });
        }
        view::render_sidebar(state, actions_tx)
    }

    #[test]
    fn sidebar_nsh_selects_projects_and_toggles_agents() {
        let (events_tx, events_rx) = unbounded::<SidebarSnapshot>();
        let (actions_tx, mut actions_rx) = unbounded::<SidebarAction>();
        let initial = SidebarSnapshot {
            projects: vec![
                ProjectRowSnapshot {
                    id: 7,
                    name: "kumu".into(),
                    path: "/tmp/kumu".into(),
                    kind: ProjectRowKind::RepoHeader,
                    repo_name: None,
                    active: false,
                    has_active_child: true,
                    unread: false,
                    todo_count: 2,
                    todo_attention: false,
                },
                ProjectRowSnapshot {
                    id: 7,
                    name: "main".into(),
                    path: "/tmp/kumu".into(),
                    kind: ProjectRowKind::MainWorktree,
                    repo_name: Some("kumu".into()),
                    active: true,
                    has_active_child: false,
                    unread: false,
                    todo_count: 0,
                    todo_attention: false,
                },
            ],
            agents: vec![],
            agents_collapsed: false,
            update_notice: None,
        };
        let mount_id = MOUNTS.register(SidebarMount {
            initial,
            events_rx,
            actions_tx,
        });
        let root = nsh_core::element::Root::new::<SidebarRoot>(move || SidebarRootProps {
            mount_id: mount_id.into(),
        });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start sidebar NSH");

        // Fixed-height Agent section stays visible with a calm empty state when 0 agents exist.
        app.wait_for("kumu").expect("repo header renders");
        app.wait_for("main").expect("main worktree renders");
        app.press("main").expect("click main worktree");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(SidebarAction::SelectProject(7))
        );
        app.wait_for("Agent 状态").expect("persistent agent header");
        app.wait_for("暂无运行中的 Agent")
            .expect("empty agent placeholder");

        // Populating agents renders them inside the virtualized fixed-height list.
        events_tx
            .unbounded_send(SidebarSnapshot {
                projects: vec![
                    ProjectRowSnapshot {
                        id: 7,
                        name: "kumu".into(),
                        path: "/tmp/kumu".into(),
                        kind: ProjectRowKind::RepoHeader,
                        repo_name: None,
                        active: false,
                        has_active_child: true,
                        unread: false,
                        todo_count: 2,
                        todo_attention: false,
                    },
                    ProjectRowSnapshot {
                        id: 7,
                        name: "main".into(),
                        path: "/tmp/kumu".into(),
                        kind: ProjectRowKind::MainWorktree,
                        repo_name: Some("kumu".into()),
                        active: true,
                        has_active_child: false,
                        unread: false,
                        todo_count: 0,
                        todo_attention: false,
                    },
                ],
                agents: vec![AgentRowSnapshot {
                    pane: PaneId(3),
                    project_name: "kumu".into(),
                    project_path: "/tmp/kumu".into(),
                    tab_name: "工作区 1".into(),
                    agent: "claude".into(),
                    title: "Refactor UI".into(),
                    state: Some(AgentState::Waiting),
                    focused: false,
                    unseen: false,
                }],
                agents_collapsed: false,
                update_notice: None,
            })
            .expect("push agent snapshot");

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
