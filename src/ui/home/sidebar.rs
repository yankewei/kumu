//! Left column: projects and the settings entry.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::UpdateStatus;

use super::KumuApp;
use crate::ui::tooltip::HoverLabel as _;

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
                        format!("正在下载 {}%", downloaded.saturating_mul(100) / total)
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
        let roots = projects.iter().filter(|project| {
            project
                .parent_id
                .is_none_or(|parent| !projects.iter().any(|project| project.id == parent))
        });
        for project in roots {
            tree.push(self.render_project_row(project, false, cx));
            for worktree in projects
                .iter()
                .filter(|worktree| worktree.parent_id == Some(project.id))
            {
                tree.push(self.render_project_row(worktree, true, cx));
            }
        }

        v_flex()
            .w_64()
            .h_full()
            .flex_shrink_0()
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
                div()
                    .flex_1()
                    .id("project-list")
                    .overflow_y_scroll()
                    .children(tree),
            )
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
                        h_flex().child(
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
                move |menu, _, _| {
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
                    let menu = menu.item(rename);
                    if worktree {
                        menu.item(delete)
                    } else {
                        let create = PopupMenuItem::new("新建 worktree").on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, move |app, cx| {
                                    app.prompt_create_worktree(project_id, window, cx)
                                });
                            }
                        });
                        menu.item(create).item(delete)
                    }
                }
            })
            .child(
                Icon::new(if active {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                })
                .small()
                .text_color(fg),
            )
            .child(
                div()
                    .id(format!("project-name-{project_id}"))
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_medium()
                    .text_color(fg)
                    // The tree shows the name alone; where the project
                    // actually lives is a hover label above it.
                    .hover_label(&self.tooltips, project.path.clone())
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
                            .hover_label(
                                &self.tooltips,
                                if worktree {
                                    "移除 worktree"
                                } else {
                                    "移除项目"
                                },
                            )
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
                            .hover_label(&self.tooltips, "新建终端")
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
