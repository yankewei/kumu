//! Left column: projects, sessions, and settings entry.

use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

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
        // Row context menus outlive this render, so they reach the view
        // through a weak handle.
        let weak = cx.entity().downgrade();

        // ---- Left: sidebar surface with the project/session tree. ----
        let mut tree: Vec<AnyElement> = Vec::new();
        for project in &self.projects {
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
                        cx.notify();
                    }))
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
                        div()
                            .id(format!("project-new-session-{project_id}"))
                            .rounded(radius)
                            .p_0p5()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .opacity(0.)
                            .group_hover(group, |style| style.opacity(1.))
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.new_session_in(project_id, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Plus).small()),
                    )
                    .into_any_element(),
            );
            let project_sessions = self
                .sessions
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
                    let agent = AGENTS.iter().find(|a| a.id == s.agent).and_then(|a| a.logo);
                    // A prompt can be waiting on a session the user is not
                    // looking at, so mark the row that needs an answer.
                    let waiting = self.has_pending_permission(sid, cx);
                    tree.push(
                        div()
                            .id(format!("session-{sid}"))
                            .px_3()
                            .py_1p5()
                            .cursor_pointer()
                            .bg(bg)
                            .text_color(fg)
                            .when(!active, |this| {
                                this.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_session(sid, cx);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().flex_1().text_xs().child(title))
                                    .when(waiting, |this| {
                                        this.child(
                                            div()
                                                .size(px(6.))
                                                .flex_none()
                                                .rounded(px(3.))
                                                .bg(theme.warning),
                                        )
                                    })
                                    .child(agent_logo(agent, px(14.), tag)),
                            )
                            // Right-click the row for its actions; deleting is
                            // destructive, so it asks for confirmation first.
                            .context_menu({
                                let weak = weak.clone();
                                move |menu, _, _| {
                                    let rename = PopupMenuItem::new("重命名").on_click({
                                        let weak = weak.clone();
                                        move |_, window, cx| {
                                            let _ = weak.update(cx, move |app, cx| {
                                                app.prompt_rename_session(sid, window, cx)
                                            });
                                        }
                                    });
                                    let delete = PopupMenuItem::new("删除").on_click({
                                        let weak = weak.clone();
                                        move |_, window, cx| {
                                            let _ = weak.update(cx, move |app, cx| {
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
                div()
                    .px_3()
                    .py_1p5()
                    .flex()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .child(
                        div()
                            .id("open-settings")
                            .rounded(radius)
                            .p_1()
                            .cursor_pointer()
                            .text_color(muted_fg)
                            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                            .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
                            .child(Icon::new(IconName::Settings).small()),
                    ),
            )
    }
}
