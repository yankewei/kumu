//! kumu demo: a small GPUI app that drives a local Opencode agent over ACP v1.
//!
//! Architecture (single UI thread + background ACP thread):
//!   - gpui/gpui-kit render the conversation on the main thread.
//!   - `acp` module runs a tokio runtime on a dedicated thread that owns the
//!     ACP stdio connection to `opencode acp`.
//!   - ACP -> UI uses a std mpsc channel, drained by a poll task.
//!   - UI -> ACP uses a tokio unbounded channel (non-blocking send).

mod acp;

use std::sync::mpsc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Root;
use gpui_kit::prelude::*;
use gpui_kit::*;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    User,
    Agent,
    Log,
    Error,
    Done,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::User => "▸ you   ",
            Kind::Agent => "◇ agent ",
            Kind::Log => "· sys    ",
            Kind::Error => "! err    ",
            Kind::Done => "✓ done   ",
        }
    }
    fn color(self) -> Hsla {
        match self {
            Kind::User => hsla(0.55, 0.7, 0.65, 1.0),
            Kind::Agent => hsla(0.1, 0.7, 0.72, 1.0),
            Kind::Log => hsla(0.0, 0.0, 0.55, 1.0),
            Kind::Error => hsla(0.0, 0.85, 0.55, 1.0),
            Kind::Done => hsla(0.33, 0.7, 0.6, 1.0),
        }
    }
    fn weight(self) -> FontWeight {
        match self {
            Kind::User | Kind::Agent => FontWeight::MEDIUM,
            _ => FontWeight::NORMAL,
        }
    }
}

struct KumuApp {
    input_state: Entity<InputState>,
    rows: Vec<(Kind, String)>,
    status: SharedString,
    cmd_tx: UnboundedSender<String>,
    _poll: Task<()>,
}

impl KumuApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state =
            cx.new(|cx| InputState::new(window, cx).placeholder("Ask the agent… (Send button)"));

        let (ev_tx, ev_rx) = mpsc::channel::<acp::Event>();
        let cfg = acp::Config::agent();
        let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let cmd_tx = acp::spawn(cfg, cwd, ev_tx);

        // Drain the ACP -> UI channel on the main thread.
        let poll = cx.spawn(async move |this, cx| {
            loop {
                for ev in ev_rx.try_iter() {
                    this.update(cx, |this, cx| this.on_event(ev, cx)).ok();
                }
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
            }
        });

        Self {
            input_state,
            rows: Vec::new(),
            status: "connecting to opencode…".into(),
            cmd_tx,
            _poll: poll,
        }
    }

    fn on_event(&mut self, ev: acp::Event, cx: &mut Context<Self>) {
        match ev {
            acp::Event::Log(s) => {
                self.status = s.clone().into();
                self.push(Kind::Log, s);
            }
            acp::Event::Agent(s) => {
                // Streaming: append to the trailing agent row when there is one.
                if let Some((Kind::Agent, text)) = self.rows.last_mut() {
                    text.push_str(&s);
                } else {
                    self.push(Kind::Agent, s);
                }
            }
            acp::Event::Permission(s) => self.push(Kind::Log, format!("permission: {s}")),
            acp::Event::Done(s) => {
                self.status = format!("idle ({s})").into();
                self.push(Kind::Done, s);
            }
            acp::Event::Error(s) => {
                self.status = "error".into();
                self.push(Kind::Error, s);
            }
        }
        cx.notify();
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        self.rows.push((kind, text.into()));
    }

    fn send_prompt(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_state.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.push(Kind::User, text.clone());
        self.status = "running…".into();
        self.cmd_tx.send(text).ok();
        cx.notify();
    }
}

impl Render for KumuApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone();
        let rows = self.rows.clone();

        let messages = div()
            .flex()
            .flex_col()
            .p(px(12.))
            .gap(px(4.))
            .children(
                rows.into_iter()
                    .map(|(kind, text)| {
                        let color = kind.color();
                        let weight = kind.weight();
                        div()
                            .w_full()
                            .text_color(color)
                            .font_weight(weight)
                            .child(format!("{}{}", kind.prefix(), text))
                    })
            );

        let send_button = Button::new("send").primary().label("Send").on_click(cx.listener(
            |this, _, window, cx| this.send_prompt(window, cx),
        ));

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(12.))
                    .py(px(8.))
                    .gap(px(8.))
                    .child(div().text_size(px(13.)).text_color(hsla(0., 0., 0.55, 1.)).child("kumu"))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(hsla(0., 0., 0.45, 1.))
                            .child(status),
                    ),
            )
            .child(div().flex_1().id("msg-scroll").overflow_y_scroll().child(messages))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(12.))
                    .py(px(8.))
                    .gap(px(8.))
                    .child(Input::new(&self.input_state).flex_1())
                    .child(send_button),
            )
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(900.), px(700.)), cx)),
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
