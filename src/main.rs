//! kumu — local multi-agent desktop shell.
//!
//! Follows GPUI Kit's design guides:
//!   - layout uses rem helpers (w_64/p_4/gap_3/text_sm…), px only for physical
//!     boundaries (hairlines via h_px/w_px, window chrome);
//!   - colors/surfaces read from cx.theme() by semantic role (sidebar uses
//!     sidebar_* tokens; no raw hex/hsla in app code);
//!   - density/spacing/typography stay on the semantic scale;
//!   - primary reserved for the decision commit, internal nav = ghost/default.
//!
//! Two screens in one window: Home (projects → sessions tree + conversation)
//! and Settings (category nav + supported-agents list). Persistence is local
//! SQLite (~/.kumu/kumu.db). Real-time ACP wiring is the next step.

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

#[derive(Clone, Copy, PartialEq, Debug)]
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
                    (
                        theme.sidebar_accent,
                        theme.sidebar_accent_foreground,
                        theme.sidebar_accent_foreground,
                    )
                } else {
                    (theme.transparent, theme.sidebar_foreground, theme.muted_foreground)
                };
                let sid = s.id;
                let title = s.title.clone();
                let agent = self.agent_name(&s.agent);
                tree.push(
                    div()
                        .id(format!("session-{sid}"))
                        .mx_2()
                        .px_3()
                        .py_1()
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
                                .child(div().text_xs().text_color(tag).child(agent)),
                        ),
                );
            }
        }

        let left = v_flex()
            .w_64()
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex()
                    .px_4()
                    .h_10()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().font_bold().child("kumu"))
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
            .child(div().h_px().w_full().bg(theme.sidebar_border))
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
                    .border_color(theme.sidebar_border)
                    .child(
                        Button::new("new-session")
                            .label("+ 新建会话")
                            .small()
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
                // Owned copies so the row closure is 'static (no theme borrow).
                let (primary, muted, foreground, radius) =
                    (theme.primary, theme.muted, theme.foreground, theme.radius);
                let rows = v_flex()
                    .gap_3()
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
                                            .text_xs()
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
                            .gap_3()
                            .px_4()
                            .h_12()
                            .child(div().text_sm().font_bold().child(title))
                            .child(
                                div()
                                    .px_2()
                                    .rounded_full()
                                    .bg(theme.muted)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(agent),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(cwd),
                            ),
                    )
                    .child(div().h_px().w_full().bg(theme.border))
                    .child(
                        div()
                            .flex_1()
                            .id("conversation")
                            .overflow_y_scroll()
                            .px_4()
                            .py_3()
                            .child(rows),
                    )
                    .child(div().h_px().w_full().bg(theme.border))
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
                .text_color(theme.muted_foreground)
                .child(div().text_sm().child("还没有会话"))
                .child(
                    Button::new("empty-new")
                        .label("新建会话")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.new_session();
                            cx.notify();
                        })),
                ),
        };

        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(left)
            .child(div().w_px().h_full().bg(theme.border))
            .child(right)
    }

    fn render_settings(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();

        let nav_item = |label: &str, active: bool| {
            div()
                .id(format!("settings-nav-{label}"))
                .w_full()
                .px_3()
                .py_1()
                .rounded(theme.radius)
                .bg(if active { theme.sidebar_accent } else { theme.transparent })
                .text_color(if active { theme.sidebar_accent_foreground } else { theme.muted_foreground })
                .text_sm()
                .child(label.to_string())
        };

        let left = v_flex()
            .w_56()
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .child(
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_4()
                    .child(nav_item("服务商", true))
                    .child(nav_item("通用", false))
                    .child(nav_item("快捷键", false)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        Button::new("back-home")
                            .ghost()
                            .label("返回首页")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Home;
                                cx.notify();
                            })),
                    ),
            );

        // Agent entries: label + description, with a trailing neutral status.
        let rows = v_flex()
            .gap_2()
            .children(AGENTS.iter().map(|a| {
                h_flex()
                    .items_center()
                    .gap_4()
                    .py_1()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().text_sm().font_bold().child(a.name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(a.desc),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("本地"),
                    )
            }));

        let right = v_flex()
            .flex_1()
            .h_full()
            .px_6()
            .py_8()
            .child(
                div()
                    .w_full()
                    .max_w_80()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().text_lg().font_bold().child("服务商"))
                            .child(div().text_sm().text_color(theme.muted_foreground).child("本机可用的 agent"))
                            .child(div().h_px().w_full().bg(theme.border).mt_4())
                            .child(rows),
                    ),
            );

        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
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
            match cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| KumuApp::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            }) {
                Ok(_) => eprintln!("[ui] window opened"),
                Err(e) => eprintln!("[ui] window open FAILED: {e:?}"),
            }
        })
        .detach();
    });
}
