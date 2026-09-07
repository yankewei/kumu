//! kumu — local multi-agent desktop shell.
//!
//! Two screens (same window, view switch):
//!   Home     : left tree (projects → sessions), settings entry in the header;
//!              right panel shows the selected session's conversation.
//!   Settings : left nav ("服务商"), right shows the supported agents.
//!
//! Sessions/messages persist to a local SQLite DB (~/.kumu/kumu.db). Light
//! theme (default). Real-time ACP prompt/response is the next step; the input
//! currently records user messages locally (agent reply wiring pending).

mod db;

use std::path::PathBuf;

use db::{Message, Project, Session, Store};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

/// Agents kumu can drive (static registry; ACP wiring is the next step).
struct AgentDef {
    id: &'static str,
    name: &'static str,
    desc: &'static str,
}

const AGENTS: [AgentDef; 2] = [
    AgentDef {
        id: "opencode",
        name: "Opencode",
        desc: "SST Opencode · ACP v1",
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        desc: "omp · ACP v1",
    },
];

#[derive(Clone, Copy, PartialEq)]
enum View {
    Home,
    Settings,
}

struct KumuApp {
    store: Store,
    view: View,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    selected: Option<i64>,
    messages: Vec<Message>,
    input_state: Entity<InputState>,
}

impl KumuApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let db_path = db_path();
        let store = Store::open(&db_path).expect("failed to open sqlite db");
        let mut app = Self {
            store,
            view: View::Home,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected: None,
            messages: Vec::new(),
            input_state: cx.new(|cx| InputState::new(window, cx).placeholder("输入消息…")),
        };
        app.bootstrap();
        app
    }

    /// Make sure there is at least one project + one session so the shell is usable.
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
}

impl KumuApp {
    fn render_home(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let selected = self.selected;

        // Left column: projects -> sessions tree.
        let mut tree: Vec<Stateful<Div>> = Vec::new();
        for project in &self.projects {
            let project_name = project.name.clone();
            tree.push(
                div()
                    .id(format!("project-head-{}", project.id))
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_size(px(11.))
                    .font_bold()
                    .text_color(theme.muted_foreground)
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
                    (theme.primary, theme.primary_foreground, theme.primary_foreground)
                } else {
                    (theme.transparent, theme.foreground, theme.muted_foreground)
                };
                let sid = s.id;
                let title = s.title.clone();
                let agent = self.agent_name(&s.agent);
                tree.push(
                    div()
                        .id(format!("session-{sid}"))
                        .mx_2()
                        .px_3()
                        .py(px(6.))
                        .rounded(theme.radius)
                        .bg(bg)
                        .text_color(fg)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_session(sid);
                            cx.notify();
                        }))
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().text_sm().child(title))
                                .child(div().text_size(px(10.)).text_color(tag).child(agent)),
                        ),
                );
            }
        }

        let left = v_flex()
            .w(px(264.))
            .h_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .items_center()
                    .justify_between()
                    .child(div().font_bold().child("kumu"))
                    .child(
                        div()
                            .id("open-settings")
                            .rounded(theme.radius)
                            .p_1()
                            .text_color(theme.muted_foreground)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Settings;
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Settings)),
                    ),
            )
            .child(div().h_px().w_full().bg(theme.border))
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
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("new-session")
                            .ghost()
                            .small()
                            .label("+ 新建会话")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_session();
                                cx.notify();
                            })),
                    ),
            );

        // Right: selected session's agent header + conversation.
        let right = match self.selected_session() {
            Some(session) => {
                let agent = self.agent_name(&session.agent);
                let title = session.title.clone();
                let cwd = session.cwd.clone();
                let messages = self.messages.clone();
                let agent_for_rows = agent.clone();
                // Owned copies so the row closure is 'static (no theme borrow).
                let (primary, muted, foreground, radius) =
                    (theme.primary, theme.muted, theme.foreground, theme.radius);
                let rows = v_flex()
                    .gap_2()
                    .children(messages.into_iter().map(move |m| {
                        let user = m.role == "user";
                        let (bg, who) = if user {
                            (primary.opacity(0.08), "你".to_string())
                        } else {
                            (muted, agent_for_rows.clone())
                        };
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .rounded(radius)
                            .bg(bg)
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .font_bold()
                                            .text_color(if user { primary } else { foreground })
                                            .child(who),
                                    )
                                    .child(div().text_sm().child(m.content)),
                            )
                    }));
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().text_sm().font_bold().child(title))
                            .child(
                                div()
                                    .px_2()
                                    .py(px(2.))
                                    .rounded(px(999.))
                                    .bg(theme.muted)
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child(agent),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child(cwd),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .id("conversation")
                            .overflow_y_scroll()
                            .px_4()
                            .py_3()
                            .child(rows),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .py_3()
                            .border_t_1()
                            .border_color(theme.border)
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
                .text_color(theme.muted_foreground)
                .child("选择左侧会话，或新建一个"),
        };

        h_flex()
            .size_full()
            .bg(theme.background)
            .child(left)
            .child(div().w_px().h_full().bg(theme.border))
            .child(right)
    }

    fn render_settings(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();

        let rows = v_flex()
            .gap_2()
            .children(AGENTS.iter().map(|a| {
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .child(div().font_medium().text_sm().child(a.name))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(a.desc),
                            ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py(px(2.))
                            .rounded(px(999.))
                            .bg(theme.muted)
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("本地"),
                    )
            }));

        let left = v_flex()
            .w(px(220.))
            .h_full()
            .bg(theme.background)
            .child(div().px_4().py_3().font_bold().text_sm().child("设置"))
            .child(div().h_px().w_full().bg(theme.border))
            .child(
                v_flex()
                    .gap(px(2.))
                    .px_3()
                    .py_3()
                    .child(
                        div()
                            .px_3()
                            .py(px(6.))
                            .rounded(theme.radius)
                            .bg(theme.primary)
                            .text_color(theme.primary_foreground)
                            .text_sm()
                            .child("服务商"),
                    )
                    .child(
                        div()
                            .px_3()
                            .py(px(6.))
                            .rounded(theme.radius)
                            .text_color(theme.muted_foreground)
                            .text_sm()
                            .child("通用"),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px_3()
                    .py_3()
                    .child(
                        Button::new("back-home")
                            .ghost()
                            .label("← 返回首页")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Home;
                                cx.notify();
                            })),
                    ),
            );

        let right = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .px_5()
            .py_5()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_lg().font_bold().child("服务商"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("本机可用的 agent"),
                    ),
            )
            .child(rows);

        h_flex()
            .size_full()
            .bg(theme.background)
            .child(left)
            .child(div().w_px().h_full().bg(theme.border))
            .child(right)
    }
}

impl Render for KumuApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.view {
            View::Home => self.render_home(window, cx),
            View::Settings => self.render_settings(window, cx),
        }
    }
}

fn db_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".kumu").join("kumu.db")
}

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1040.), px(680.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| KumuApp::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
