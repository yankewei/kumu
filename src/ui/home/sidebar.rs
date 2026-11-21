//! Left column: projects and the settings entry.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::UpdateStatus;

use super::KumuApp;
use super::panes::PaneId;
use crate::agent::AgentState;
use crate::ui::tooltip::HoverLabel as _;

/// One live agent row in the global sidebar list.
pub(super) struct AgentRow {
    pane: PaneId,
    project_name: String,
    project_path: String,
    tab_name: String,
    agent: String,
    title: String,
    state: Option<AgentState>,
    focused: bool,
}

impl KumuApp {
    pub(super) fn render_sidebar(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted_fg, radius) = (theme.muted_foreground, theme.radius);
        let (sidebar, sidebar_fg, sidebar_border) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
        );
        let sidebar_accent = theme.sidebar_accent;
        let updater = self.updater.read(cx);
        let update_notice = match updater.status() {
            UpdateStatus::Available(version) => {
                Some((IconName::ArrowUp, format!("新版本 {version}")))
            }
            UpdateStatus::Downloading { downloaded, total } => {
                let label = match total {
                    Some(total) if *total > 0 => {
                        format!(
                            "正在下载 {}%",
                            (downloaded.saturating_mul(100) / total).min(100)
                        )
                    }
                    _ => "正在下载更新…".to_string(),
                };
                Some((IconName::LoaderCircle, label))
            }
            UpdateStatus::Installing => Some((IconName::LoaderCircle, "正在安装更新…".to_string())),
            UpdateStatus::Staged(_) => Some((IconName::Check, "重启以完成更新".to_string())),
            UpdateStatus::Errored(_) if updater.available().is_some() => {
                Some((IconName::TriangleAlert, "更新失败，查看详情".to_string()))
            }
            _ => None,
        };
        let mut tree: Vec<AnyElement> = Vec::new();
        let workspace = self.workspace.read(cx);
        let projects = workspace.projects();
        // Worktrees hang under the project they were created from. A row
        // whose parent is gone (a database trimmed outside Kumu) still shows
        // at the top level instead of disappearing.
        let mut rendered = std::collections::HashSet::with_capacity(projects.len());
        let roots = projects.iter().filter(|project| {
            project.parent_id.is_none_or(|parent| {
                parent == project.id || !projects.iter().any(|candidate| candidate.id == parent)
            })
        });
        for project in roots {
            if rendered.insert(project.id) {
                tree.push(self.render_project_row(project, false, cx));
            }
            for worktree in projects
                .iter()
                .filter(|worktree| worktree.parent_id == Some(project.id))
            {
                if rendered.insert(worktree.id) {
                    tree.push(self.render_project_row(worktree, true, cx));
                }
            }
        }
        // Any remaining rows (e.g. a corrupted parent_id cycle) still appear
        // at top level instead of vanishing from the sidebar.
        for project in projects {
            if rendered.insert(project.id) {
                tree.push(self.render_project_row(project, false, cx));
            }
        }

        // The agent section floats between the projects and the footer,
        // slightly below the midpoint: two parts of free space above it and
        // one below. Without agents a single spacer keeps the footer down.
        let agent_zone: Vec<AnyElement> = match self.render_agents_section(cx) {
            Some(section) => vec![
                div().flex_grow(2.).min_h_0().into_any_element(),
                section,
                div().flex_grow(1.).min_h_0().into_any_element(),
            ],
            None => vec![div().flex_1().min_h_0().into_any_element()],
        };

        v_flex()
            // The resizable panel beside the workspace owns the width; the
            // column just fills it.
            .w_full()
            .h_full()
            .bg(sidebar)
            .text_color(sidebar_fg)
            .child(
                h_flex()
                    .id("sidebar-header")
                    .px_3()
                    .py_1p5()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(sidebar_border)
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(muted_fg)
                            .child("项目"),
                    )
                    .child(
                        div()
                            .id("add-project")
                            .rounded(radius)
                            .p_0p5()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.add_project(cx);
                            }))
                            .child(Icon::new(IconName::Plus).small()),
                    ),
            )
            .child(
                // Content height, so the flexible spacers around the Agent
                // section decide where it sits; a long project list scrolls
                // instead of pushing the footer down.
                div()
                    .flex_shrink_1()
                    .min_h_0()
                    .id("project-list")
                    .overflow_y_scroll()
                    .children(tree),
            )
            .children(agent_zone)
            .child(
                v_flex()
                    .px_3()
                    .py_1p5()
                    .gap_1()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .when_some(update_notice, |footer, (icon, label)| {
                        footer.child(
                            Button::new("sidebar-update")
                                .small()
                                .ghost()
                                .w_full()
                                .justify_start()
                                .text_color(theme.link)
                                .icon(icon)
                                .label(label)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_settings(true, cx);
                                })),
                        )
                    })
                    .child(
                        h_flex().w_full().justify_between().child(
                            div()
                                .id("open-settings")
                                .rounded(radius)
                                .p_1()
                                .cursor_pointer()
                                .text_color(muted_fg)
                                .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.open_settings(false, cx)),
                                )
                                .child(Icon::new(IconName::Settings).small()),
                        ),
                    ),
            )
    }

    /// Every live agent across all projects and worktrees, across all tabs.
    /// Waiting comes first, idle last; panes in the same state keep project
    /// tree and layout order.
    fn agent_rows(&self, cx: &Context<Self>) -> Vec<AgentRow> {
        let workspace = self.workspace.read(cx);
        let projects = workspace.projects();
        let mut ordered = Vec::with_capacity(projects.len());
        let mut seen = std::collections::HashSet::with_capacity(projects.len());
        let roots = projects.iter().filter(|project| {
            project.parent_id.is_none_or(|parent| {
                parent == project.id || !projects.iter().any(|candidate| candidate.id == parent)
            })
        });
        for project in roots {
            if seen.insert(project.id) {
                ordered.push(project);
            }
            for worktree in projects
                .iter()
                .filter(|worktree| worktree.parent_id == Some(project.id))
            {
                if seen.insert(worktree.id) {
                    ordered.push(worktree);
                }
            }
        }
        for project in projects {
            if seen.insert(project.id) {
                ordered.push(project);
            }
        }

        let mut rows = Vec::new();
        for project in ordered {
            let Some(tabs) = self.tabs_by_project.get(&project.id) else {
                continue;
            };
            for (index, tab) in tabs.tabs.iter().enumerate() {
                let tab_name = Self::tab_name(tabs, index);
                for pane in tab.layout.panes() {
                    let Some(entry) = self.agent_status.get(&pane) else {
                        continue;
                    };
                    rows.push(AgentRow {
                        pane,
                        project_name: project.name.clone(),
                        project_path: project.path.clone(),
                        tab_name: tab_name.clone(),
                        agent: entry.agent.clone(),
                        title: entry.display_title(),
                        state: entry.state,
                        focused: self.selected == Some(project.id) && self.focused == pane,
                    });
                }
            }
        }
        rows.sort_by_key(|row| row.state.map_or(4, AgentState::priority));
        rows
    }

    /// The global agent section between the project list and settings footer.
    /// Disappears only when no live agent exists in any project.
    fn render_agents_section(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let rows = self.agent_rows(cx);
        if rows.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted_fg, sidebar_fg, sidebar_border, sidebar_accent) = (
            theme.muted_foreground,
            theme.sidebar_foreground,
            theme.sidebar_border,
            theme.sidebar_accent,
        );
        let collapsed = self.agents_collapsed;
        let count = rows.len();

        let mut header = h_flex()
            .id("agents-header")
            .w_full()
            .h(px(32.))
            .px_3()
            .gap_1p5()
            .flex_none()
            .items_center()
            .cursor_pointer()
            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.agents_collapsed = !this.agents_collapsed;
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .font_semibold()
                    .text_color(muted_fg)
                    .child("Agent 状态"),
            )
            .child(div().flex_1());
        if collapsed {
            header = header.child(render_agent_summary(&rows, theme));
        } else {
            header = header.child(
                div()
                    .text_xs()
                    .font_medium()
                    .text_color(muted_fg)
                    .child(count.to_string()),
            );
        }
        header = header.child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .with_size(px(12.))
            .text_color(muted_fg),
        );

        // The section's height is content-driven, so the list must not use
        // `flex_1` (flex-basis 0 would collapse it to nothing). Let it take
        // its content height and only shrink when the 40% cap is reached.
        let mut list = v_flex()
            .id("agent-list")
            .w_full()
            .flex_shrink_1()
            .min_h_0()
            .overflow_y_scroll();
        if !collapsed {
            for row in rows {
                list = list.child(self.render_agent_row(row, sidebar_fg, cx));
            }
        }

        Some(
            v_flex()
                .w_full()
                .flex_none()
                // The design caps the section at about 40% of the sidebar;
                // a long list scrolls instead of squeezing the projects out.
                .max_h(relative(0.4))
                .overflow_hidden()
                .child(div().w_full().h(px(1.)).flex_none().bg(sidebar_border))
                .child(header)
                .when(!collapsed, |section| section.child(list))
                .child(div().w_full().h(px(1.)).flex_none().bg(sidebar_border))
                .into_any_element(),
        )
    }

    fn render_agent_row(&self, row: AgentRow, sidebar_fg: Hsla, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted_fg, sidebar_accent, sidebar_accent_fg) = (
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.sidebar_accent_foreground,
        );
        let color = match row.state {
            Some(AgentState::Running) => theme.link,
            Some(AgentState::Waiting) => theme.warning,
            Some(AgentState::Completed) => theme.success,
            Some(AgentState::Failed) => theme.danger,
            None => muted_fg,
        };
        let title_color = if row.focused {
            sidebar_accent_fg
        } else if matches!(row.state, None | Some(AgentState::Completed)) {
            muted_fg
        } else {
            sidebar_fg
        };
        let hover = if row.project_path.is_empty() {
            format!("{}\n{} · {}", row.title, row.project_name, row.tab_name)
        } else {
            format!(
                "{}\n{} · {}\n{}",
                row.title, row.project_name, row.tab_name, row.project_path
            )
        };
        let pane = row.pane;
        let status = row.state;

        h_flex()
            .id(format!("agent-row-{}", pane.0))
            .w_full()
            .h(px(40.))
            .flex_none()
            .px_3()
            .gap_2()
            .items_center()
            .cursor_pointer()
            .when(row.focused, |el| el.bg(sidebar_accent))
            .when(!row.focused, |el| {
                el.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
            })
            .hover_label(hover)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.reveal_agent(pane, window, cx);
            }))
            .child(agent_status_glyph(status, color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_medium()
                            .text_color(title_color)
                            .child(row.title),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .h(px(16.))
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(row.agent),
                            )
                            .child(div().flex_none().text_xs().text_color(muted_fg).child("·"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(row.project_name),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Switch to the row's project/tab and focus its pane without clearing its
    /// status: a live agent stays in the list for its entire lifetime.
    pub(super) fn reveal_agent(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(pane, cx);
        self.focus_terminal(window, cx);
        cx.notify();
    }

    /// One project or worktree row. A worktree is indented under its
    /// repository and cannot create a worktree of its own.
    fn render_project_row(
        &self,
        project: &crate::db::Project,
        worktree: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted_fg, radius) = (theme.muted_foreground, theme.radius);
        let (sidebar_fg, sidebar_accent, sidebar_accent_fg) = (
            theme.sidebar_foreground,
            theme.sidebar_accent,
            theme.sidebar_accent_foreground,
        );
        let project_name = project.name.clone();
        let project_id = project.id;
        let active = self.selected == Some(project_id);
        let (bg, fg) = if active {
            (sidebar_accent, sidebar_accent_fg)
        } else {
            (theme.transparent, sidebar_fg)
        };
        let group = format!("project-row-{project_id}");
        // Row context menus outlive this render, so they reach the view
        // through a weak handle.
        let weak = cx.entity().downgrade();

        h_flex()
            .id(format!("project-head-{project_id}"))
            .group(group.clone())
            .mt_2()
            .px_3()
            .py_1p5()
            .gap_1()
            .cursor_pointer()
            .bg(bg)
            // A worktree sits under the repository it came from.
            .when(worktree, |row| row.mt_0p5().pl_7())
            .when(!active, |row| {
                row.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_project(project_id, cx);
                this.focus_terminal(window, cx);
                cx.notify();
            }))
            // Right-click the project for its own actions; deleting it
            // closes its terminals, so it confirms first.
            .context_menu({
                let weak = weak.clone();
                let path = project.path.clone();
                let name = project_name.clone();
                move |menu, window, cx| {
                    let new_terminal = PopupMenuItem::new("新建终端").on_click({
                        let weak = weak.clone();
                        move |_, window, cx| {
                            let _ = weak.update(cx, move |app, cx| {
                                app.add_terminal(Some(project_id), cx);
                                app.focus_terminal(window, cx);
                            });
                        }
                    });
                    let rename = PopupMenuItem::new("重命名").on_click({
                        let weak = weak.clone();
                        move |_, window, cx| {
                            let _ = weak.update(cx, move |app, cx| {
                                app.prompt_rename_project(project_id, window, cx)
                            });
                        }
                    });
                    let delete = PopupMenuItem::new(if worktree { "移除" } else { "删除" })
                        .on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, move |app, cx| {
                                    app.confirm_delete_project(project_id, window, cx)
                                });
                            }
                        });
                    let reveal = PopupMenuItem::new("在 Finder 中打开").on_click({
                        let path = path.clone();
                        move |_, _, cx| reveal_in_finder(path.clone(), cx)
                    });
                    let copy_path = path.clone();
                    let copy_name = name.clone();
                    let mut menu = menu.item(new_terminal);
                    if !worktree {
                        let create = PopupMenuItem::new("新建 worktree").on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, move |app, cx| {
                                    app.prompt_create_worktree(project_id, window, cx)
                                });
                            }
                        });
                        let open = PopupMenuItem::new("打开已有 worktree").on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, move |app, cx| {
                                    app.prompt_open_worktree(project_id, window, cx)
                                });
                            }
                        });
                        menu = menu.item(create).item(open);
                    }
                    menu.item(reveal)
                        .submenu("复制", window, cx, move |menu, _, _| {
                            let path = copy_path.clone();
                            let name = copy_name.clone();
                            menu.item(PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                            }))
                            .item(
                                PopupMenuItem::new("复制名称").on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                                }),
                            )
                        })
                        .separator()
                        .item(rename)
                        .separator()
                        .item(delete)
                }
            })
            .child(
                // A worktree is a checkout of its own, so it wears the branch
                // mark. The repository it came from keeps the folder.
                if worktree {
                    Icon::empty().path("icons/git-branch.svg")
                } else {
                    Icon::new(if active {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    })
                }
                .small()
                .text_color(fg),
            )
            .child(
                div()
                    .id(format!("project-name-{project_id}"))
                    .flex_1()
                    .min_w_0()
                    // A long name ends in an ellipsis instead of wrapping onto a
                    // second line and growing the row.
                    .truncate()
                    .text_xs()
                    .font_medium()
                    .text_color(fg)
                    // The tree shows the name alone; where the project
                    // actually lives is a hover label above it.
                    .hover_label(project.path.clone())
                    .child(project_name),
            )
            .when(
                self.unread_notifications
                    .values()
                    .any(|project| *project == project_id),
                |row| row.child(div().size_1p5().flex_none().rounded_full().bg(theme.link)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .child(
                        div()
                            .id(format!("project-remove-{project_id}"))
                            .rounded(radius)
                            .p_0p5()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .opacity(0.)
                            .group_hover(group.clone(), |style| style.opacity(1.))
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .hover_label(if worktree {
                                "移除 worktree"
                            } else {
                                "移除项目"
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_project(project_id, window, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Close).small()),
                    )
                    .child(
                        div()
                            .id(format!("project-new-terminal-{project_id}"))
                            .rounded(radius)
                            .p_0p5()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .opacity(0.)
                            .group_hover(group, |style| style.opacity(1.))
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .hover_label("新建终端")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_terminal(Some(project_id), cx);
                                this.focus_terminal(window, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::SquareTerminal).small()),
                    ),
            )
            .into_any_element()
    }
}

/// The collapsed section's color-coded one-line summary, e.g. `1 等待 · 1 失败`.
/// Summarizes active states first; falls back to the idle total when all agents
/// are idle.
fn render_agent_summary(rows: &[AgentRow], theme: &Theme) -> AnyElement {
    let mut waiting = 0;
    let mut failed = 0;
    let mut running = 0;
    let mut completed = 0;
    for row in rows {
        match row.state {
            Some(AgentState::Waiting) => waiting += 1,
            Some(AgentState::Failed) => failed += 1,
            Some(AgentState::Running) => running += 1,
            Some(AgentState::Completed) => completed += 1,
            None => {}
        }
    }
    let muted_fg = theme.muted_foreground;
    let parts: Vec<(String, Hsla)> = [
        (waiting, "等待", theme.warning),
        (failed, "失败", theme.danger),
        (running, "运行中", theme.link),
        (completed, "已完成", theme.success),
    ]
    .into_iter()
    .filter(|(count, _, _)| *count > 0)
    .map(|(count, label, color)| (format!("{count} {label}"), color))
    .collect();

    if parts.is_empty() {
        return div()
            .flex_none()
            .text_xs()
            .font_medium()
            .text_color(muted_fg)
            .child(format!("{} 空闲", rows.len()))
            .into_any_element();
    }

    let mut line = h_flex().min_w_0().overflow_hidden().gap_1().items_center();
    for (index, (text, color)) in parts.into_iter().enumerate() {
        if index > 0 {
            line = line.child(div().flex_none().text_xs().text_color(muted_fg).child("·"));
        }
        line = line.child(
            div()
                .flex_none()
                .text_xs()
                .font_medium()
                .text_color(color)
                .child(text),
        );
    }
    line.into_any_element()
}

/// A 12px slot holding the state's dot, hollow idle ring, or glyph.
fn agent_status_glyph(state: Option<AgentState>, color: Hsla) -> AnyElement {
    let glyph = match state {
        Some(AgentState::Running) => gpui_kit::component::spinner::Spinner::new()
            .with_size(px(12.))
            .color(color)
            .into_any_element(),
        Some(AgentState::Waiting) => div()
            .size(px(7.))
            .rounded_full()
            .bg(color)
            .into_any_element(),
        Some(AgentState::Completed) => Icon::new(IconName::Check)
            .with_size(px(12.))
            .text_color(color)
            .into_any_element(),
        Some(AgentState::Failed) => Icon::new(IconName::TriangleAlert)
            .with_size(px(12.))
            .text_color(color)
            .into_any_element(),
        None => div()
            .size(px(7.))
            .rounded_full()
            .border(px(1.5))
            .border_color(color)
            .into_any_element(),
    };
    h_flex()
        .size(px(12.))
        .flex_none()
        .items_center()
        .justify_center()
        .child(glyph)
        .into_any_element()
}

/// Show `path` in a Finder window without blocking the UI thread. `open`
/// handles files and directories; the app is macOS-only, so there is no other
/// platform to branch on.
fn reveal_in_finder(path: String, cx: &mut App) {
    cx.background_spawn(async move {
        // Reap the child instead of leaving `open` as a zombie until quit.
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("--")
            .arg(path)
            .status();
    })
    .detach();
}
