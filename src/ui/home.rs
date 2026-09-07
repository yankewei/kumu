//! Home window: projects → sessions tree on the left, the selected session's
//! conversation on the right. Owns the settings window handle.

use std::path::PathBuf;

use super::SettingsWindow;
use crate::agents::AGENTS;
use crate::db::{db_path, Message, Project, Session, Store};
use crate::ui::widgets::pill;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

pub struct KumuApp {
    store: Store,
    settings_window: Option<WindowHandle<Root>>,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    selected: Option<i64>,
    messages: Vec<Message>,
    input_state: Entity<InputState>,
}

impl KumuApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Store::open(&db_path()).expect("failed to open sqlite db");
        let mut app = Self {
            store,
            settings_window: None,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected: None,
            messages: Vec::new(),
            input_state: cx.new(|cx| InputState::new(window, cx).placeholder("输入消息…")),
        };
        app.bootstrap();
        app
    }

    fn bootstrap(&mut self) {
        if self.store.projects().is_empty() {
            let cwd = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .to_string_lossy()
                .into_owned();
            self.store.insert_project("kumu", &cwd).ok();
        }
        self.refresh();
        if self.sessions.is_empty() {
            if let Some(p) = self.projects.first().cloned() {
                let cwd = p.path.clone();
                self.store.insert_session(p.id, "omp", "会话 1", &cwd).ok();
                self.refresh();
            }
        }
        if self.selected.is_none() {
            if let Some(s) = self.sessions.first().cloned() {
                self.select_session(s.id);
            }
        }
    }

    fn refresh(&mut self) {
        self.projects = self.store.projects();
        self.sessions = self.store.sessions();
    }

    fn select_session(&mut self, id: i64) {
        self.selected = Some(id);
        self.messages = self.store.messages(id);
    }

    fn new_session(&mut self) {
        let project_id = self
            .projects
            .first()
            .map(|p| p.id)
            .unwrap_or_else(|| self.store.insert_project("kumu", "cwd-not-set").unwrap_or(1));
        let cwd = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let n = self.sessions.len() + 1;
        if let Ok(id) = self.store.insert_session(project_id, "omp", &format!("会话 {n}"), &cwd) {
            self.refresh();
            self.select_session(id);
        }
    }

    fn send(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected else { return };
        let text = self.input_state.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.store.insert_message(session_id, "user", text.trim()).ok();
        self.messages = self.store.messages(session_id);
        // TODO(next step): route the prompt to the session's agent over ACP and
        // persist + stream the reply here.
        cx.notify();
    }

    fn selected_session(&self) -> Option<&Session> {
        let id = self.selected?;
        self.sessions.iter().find(|s| s.id == id)
    }

    fn agent_name(&self, agent_id: &str) -> String {
        AGENTS
            .iter()
            .find(|a| a.id == agent_id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| agent_id.to_string())
    }

    /// Focus the settings window if it's open, else open a new one (resizable,
    /// opens at its minimum size).
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = &self.settings_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }
        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(size(px(860.), px(520.)), cx));
        options.window_min_size = Some(size(px(860.), px(520.)));
        if let Ok(handle) = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| SettingsWindow::new(window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            self.settings_window = Some(handle);
        }
    }

    fn render_home(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let font_family = theme.font_family.clone();
        // Owned copies so row closures are 'static (no theme borrow).
        let (foreground, muted_fg, border, radius) =
            (theme.foreground, theme.muted_foreground, theme.border, theme.radius);
        let (sidebar, sidebar_fg, sidebar_border) =
            (theme.sidebar, theme.sidebar_foreground, theme.sidebar_border);
        let (sidebar_accent, sidebar_accent_fg) =
            (theme.sidebar_accent, theme.sidebar_accent_foreground);
        let (primary, link, muted) = (theme.primary, theme.link, theme.muted);
        let selected = self.selected;

        // ---- Left: sidebar surface with the project/session tree. ----
        let mut tree: Vec<Stateful<Div>> = Vec::new();
        for project in &self.projects {
            let project_name = project.name.clone();
            tree.push(
                div()
                    .id(format!("project-head-{}", project.id))
                    .px_3()
                    .pt_4()
                    .pb_1()
                    .text_xs()
                    .font_medium()
                    .text_color(muted_fg)
                    .child(project_name),
            );
            let project_sessions = self
                .sessions
                .iter()
                .filter(|s| s.project_id == project.id)
                .cloned()
                .collect::<Vec<_>>();
            for s in project_sessions {
                let active = selected == Some(s.id);
                let (bg, fg, tag) = if active {
                    (sidebar_accent, sidebar_accent_fg, sidebar_accent_fg)
                } else {
                    (theme.transparent, sidebar_fg, muted_fg)
                };
                let sid = s.id;
                let title = s.title.clone();
                let agent = self.agent_name(&s.agent);
                tree.push(
                    div()
                        .id(format!("session-{sid}"))
                        .mx_2()
                        .px_3()
                        .py_1p5()
                        .rounded(radius)
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
                                .child(div().flex_1().text_sm().child(title))
                                .child(div().text_xs().text_color(tag).child(agent)),
                        ),
                );
            }
        }

        let left = v_flex()
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
                    .p_3()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .child(
                        Button::new("new-session")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label("新建会话")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_session();
                                cx.notify();
                            })),
                    ),
            );

        // ---- Right: selected session's header + conversation. ----
        let right = match self.selected_session() {
            Some(session) => {
                let agent = self.agent_name(&session.agent);
                let title = session.title.clone();
                let cwd = session.cwd.clone();
                let messages = self.messages.clone();
                let agent_for_rows = agent.clone();
                let rows = v_flex()
                    .gap_3()
                    .children(messages.into_iter().map(move |m| {
                        if m.role == "user" {
                            h_flex().justify_end().child(
                                div()
                                    .max_w(relative(0.78))
                                    .px_3()
                                    .py_2()
                                    .rounded(radius)
                                    .bg(primary.opacity(0.1))
                                    .child(div().text_sm().child(m.content)),
                            )
                        } else {
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .text_color(muted_fg)
                                        .child(agent_for_rows.clone()),
                                )
                                .child(div().text_sm().child(m.content))
                        }
                    }));
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .h_12()
                            .child(div().text_sm().font_medium().child(title))
                            .child(pill(true, agent.clone(), radius, primary, link, theme.muted, muted_fg))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(cwd),
                            ),
                    )
                    .child(div().h_px().w_full().bg(border))
                    .child(
                        div()
                            .flex_1()
                            .id("conversation")
                            .overflow_y_scroll()
                            .px_5()
                            .py_4()
                            .child(rows),
                    )
                    .child(div().h_px().w_full().bg(border))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .child(Input::new(&self.input_state).flex_1())
                            .child(
                                Button::new("send")
                                    .primary()
                                    .label("发送")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send(window, cx);
                                    })),
                            ),
                    )
            }
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(muted_fg)
                .child(div().text_sm().child("还没有会话"))
                .child(
                    Button::new("empty-new")
                        .ghost()
                        .label("新建会话")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.new_session();
                            cx.notify();
                        })),
                ),
        };

        // Full-width title bar strip: brand on the left (traffic lights float
        // to its left), the settings gear on the right.
        let title_bar = TitleBar::new()
            .child(div().text_sm().font_bold().child("kumu"))
            .child(
                div()
                    .id("open-settings")
                    .rounded(radius)
                    .p_1()
                    .cursor_pointer()
                    .text_color(muted_fg)
                    .hover(move |style| style.bg(muted))
                    .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
                    .child(Icon::new(IconName::Settings).small()),
            );

        v_flex()
            .size_full()
            .font_family(font_family)
            .bg(theme.background)
            .text_color(foreground)
            .child(title_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(left)
                    .child(div().w_px().h_full().bg(border))
                    .child(right),
            )
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_home(window, cx)
    }
}
