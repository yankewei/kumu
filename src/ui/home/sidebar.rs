//! Left column: projects, sessions, and settings entry.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::Cancel;
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::UpdateStatus;

use super::KumuApp;
use crate::agent::AGENTS;
use crate::ui::tooltip::HoverLabel as _;
use crate::ui::widgets::agent_logo;

impl KumuApp {
    pub(super) fn render_sidebar(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted_fg, radius) = (theme.muted_foreground, theme.radius);
        let (sidebar, sidebar_fg, sidebar_border) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
        );
        let (sidebar_accent, sidebar_accent_fg) =
            (theme.sidebar_accent, theme.sidebar_accent_foreground);
        let selected = self.selected;
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
        // Row context menus outlive this render, so they reach the view
        // through a weak handle.
        let weak = cx.entity().downgrade();

        // ---- Left: sidebar surface with the project/session tree. ----
        let mut tree: Vec<AnyElement> = Vec::new();
        let workspace = self.workspace.read(cx);
        for project in workspace.projects() {
            let project_name = project.name.clone();
            let project_id = project.id;
            let collapsed = self.collapsed_projects.contains(&project_id);
            let group = format!("project-row-{project_id}");
            tree.push(
                h_flex()
                    .id(format!("project-head-{project_id}"))
                    .group(group.clone())
                    .mt_2()
                    .px_3()
                    .py_1p5()
                    .gap_1()
                    .cursor_pointer()
                    .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_projects.remove(&project_id) {
                            this.collapsed_projects.insert(project_id);
                        }
                        this.persist_workspace(cx);
                        cx.notify();
                    }))
                    // Right-click the project for its own actions; deleting it
                    // takes every session under it, so it confirms first.
                    .context_menu({
                        let weak = weak.clone();
                        move |menu, _, _| {
                            let rename = menu_action("重命名", {
                                let weak = weak.clone();
                                move |window, cx| {
                                    let _ = weak.update(cx, |app, cx| {
                                        app.prompt_rename_project(project_id, window, cx)
                                    });
                                }
                            });
                            let delete = menu_action("删除", {
                                let weak = weak.clone();
                                move |window, cx| {
                                    let _ = weak.update(cx, |app, cx| {
                                        app.confirm_delete_project(project_id, window, cx)
                                    });
                                }
                            });
                            menu.item(rename).item(delete)
                        }
                    })
                    .child(
                        Icon::new(if collapsed {
                            IconName::Folder
                        } else {
                            IconName::FolderOpen
                        })
                        .small()
                        .text_color(muted_fg),
                    )
                    .child(
                        div()
                            .id(format!("project-name-{project_id}"))
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_medium()
                            .text_color(sidebar_fg)
                            // The tree shows the name alone; where the project
                            // actually lives is a hover label above it.
                            .hover_label(&self.tooltips, project.path.clone())
                            .child(project_name),
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
                                    .hover_label(&self.tooltips, "移除项目")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_delete_project(project_id, window, cx);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(Icon::new(IconName::Close).small()),
                            )
                            .child(
                                div()
                                    .id(format!("project-new-session-{project_id}"))
                                    .rounded(radius)
                                    .p_0p5()
                                    .cursor_pointer()
                                    .text_color(muted_fg)
                                    .opacity(0.)
                                    .group_hover(group, |style| style.opacity(1.))
                                    .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                                    .hover_label(&self.tooltips, "新建终端")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.new_session(Some(project_id), cx);
                                        this.focus_terminal(window, cx);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(Icon::new(IconName::SquareTerminal).small()),
                            ),
                    )
                    .into_any_element(),
            );
            let project_sessions = workspace
                .sessions()
                .iter()
                .filter(|s| s.project_id == project.id)
                .cloned()
                .collect::<Vec<_>>();
            if !collapsed {
                for s in project_sessions {
                    let active = selected == Some(s.id);
                    let (bg, fg, tag) = if active {
                        (sidebar_accent, sidebar_accent_fg, sidebar_accent_fg)
                    } else {
                        (theme.transparent, sidebar_fg, muted_fg)
                    };
                    let sid = s.id;
                    let title = s.title.clone();
                    let title_tooltip = title.clone();
                    let shell = crate::term::is_shell(&s.agent);
                    let agent = AGENTS.iter().find(|a| a.id == s.agent).and_then(|a| a.logo);
                    tree.push(
                        div()
                            .id(format!("session-{sid}"))
                            .min_w_0()
                            .px_3()
                            .py_1p5()
                            .cursor_pointer()
                            .bg(bg)
                            .text_color(fg)
                            .when(!active, |this| {
                                this.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_session(sid, cx);
                                this.focus_terminal(window, cx);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .id(format!("session-name-{sid}"))
                                            .flex_1()
                                            .min_w_0()
                                            .text_xs()
                                            .truncate()
                                            .hover_label(&self.tooltips, title_tooltip)
                                            .child(title),
                                    )
                                    .child(div().size(px(14.)).flex_none().child(if shell {
                                        Icon::new(IconName::SquareTerminal)
                                            .with_size(px(14.))
                                            .text_color(tag)
                                            .into_any_element()
                                    } else {
                                        agent_logo(agent, px(14.), tag)
                                    })),
                            )
                            // Right-click the row for its actions; deleting is
                            // destructive, so it asks for confirmation first.
                            .context_menu({
                                let weak = weak.clone();
                                move |menu, _, _| {
                                    let rename = menu_action("重命名", {
                                        let weak = weak.clone();
                                        move |window, cx| {
                                            let _ = weak.update(cx, |app, cx| {
                                                app.prompt_rename_session(sid, window, cx)
                                            });
                                        }
                                    });
                                    let delete = menu_action("删除", {
                                        let weak = weak.clone();
                                        move |window, cx| {
                                            let _ = weak.update(cx, |app, cx| {
                                                app.confirm_delete_session(sid, window, cx)
                                            });
                                        }
                                    });
                                    menu.item(rename).item(delete)
                                }
                            })
                            .into_any_element(),
                    );
                }
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
                    .id("session-tree")
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
}

/// A context-menu row that runs on mouse-down.
///
/// gpui-kit 0.6 draws a context menu in a deferred overlay, where
/// `PopupMenuItem::on_click` never fires (longbridge/gpui-component#3134).
/// The menu is dismissed on the press so the overlay is gone before a
/// confirm dialog opens. Enter goes through the item's click handler, and
/// `fired` collapses the two paths into one dialog.
fn menu_action(
    label: &'static str,
    on_press: impl Fn(&mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    let on_press = Rc::new(on_press);
    let fired = Rc::new(Cell::new(false));
    let run = Rc::new(move |window: &mut Window, cx: &mut App| {
        if fired.replace(true) {
            return;
        }
        let on_press = on_press.clone();
        window.defer(cx, move |window, cx| on_press(window, cx));
    });
    let run_mouse = run.clone();
    PopupMenuItem::element(move |_, _| {
        let run_mouse = run_mouse.clone();
        div()
            .w_full()
            .flex_1()
            .text_sm()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.dispatch_action(Box::new(Cancel), cx);
                run_mouse(window, cx);
            })
            .child(label)
    })
    .on_click({
        let run = run.clone();
        move |_, window, cx| run(window, cx)
    })
}
