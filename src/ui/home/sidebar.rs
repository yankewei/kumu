//! Left column: projects, sessions, and settings entry.

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::KumuApp;
use crate::agents::AGENTS;
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

        // ---- Left: sidebar surface with the project/session tree. ----
        let mut tree: Vec<Stateful<Div>> = Vec::new();
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
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_medium()
                            .text_color(sidebar_fg)
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
                                this.new_session_in(project_id);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Plus).small()),
                    ),
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
                                this.select_session(sid);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().flex_1().text_xs().child(title))
                                    .child(agent_logo(agent, px(14.), tag)),
                            ),
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
