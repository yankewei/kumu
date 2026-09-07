//! kumu: settings page demo — lists the model providers a local ACP agent can
//! use, derived from the agent's session `configOptions` (category: model).
//!
//! Current scope (single agent, read-only): connects to `omp acp` (override
//! with KUMU_AGENT=opencode), creates one session, reads its model config
//! option, groups the models by provider, and renders the settings page:
//!
//! ```text
//! ┌────────────────────┬───────────────────────────────┐
//! │ 设置                │  服务商                       │
//! │  ▸ 服务商  (选中)    │  opencode-go    36 个模型     │
//! │   会话 / 外观 / …    │  rc              4 个模型     │
//! └────────────────────┴───────────────────────────────┘
//! ```

mod acp;

use std::sync::mpsc;
use std::time::Duration;

use gpui_kit::component::Root;
use gpui_kit::prelude::*;
use gpui_kit::*;
use tokio::sync::mpsc::UnboundedSender;

const SIDEBAR_W: f32 = 220.0;
const NEUTRAL: Hsla = hsla(0., 0., 0.6, 1.0);

struct KumuApp {
    status: SharedString,
    providers: Vec<(String, usize)>,
    total_models: usize,
    /// Kept alive so the ACP connection loop never sees its channel close.
    _cmd_tx: UnboundedSender<String>,
    _poll: Task<()>,
}

impl KumuApp {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            status: "connecting…".into(),
            providers: Vec::new(),
            total_models: 0,
            _cmd_tx: cmd_tx,
            _poll: poll,
        }
    }

    fn on_event(&mut self, ev: acp::Event, cx: &mut Context<Self>) {
        match ev {
            acp::Event::Log(s) => self.status = s.into(),
            acp::Event::Error(s) => self.status = format!("error: {s}").into(),
            acp::Event::Providers {
                total_models,
                providers,
            } => {
                self.providers = providers.into_iter().map(|p| (p.id, p.count)).collect();
                self.total_models = total_models;
                self.status = "connected".into();
            }
            // Chat-only events are not rendered in the settings page.
            acp::Event::Agent(_) | acp::Event::Done(_) | acp::Event::Permission(_) => {}
        }
        cx.notify();
    }
}

impl Render for KumuApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone();
        let providers = self.providers.clone();
        let provider_count = providers.len();
        let total_models = self.total_models;
        let agent = std::env::var("KUMU_AGENT").unwrap_or_else(|_| "omp".into());

        let sidebar_items = [
            ("服务商", true),
            ("会话", false),
            ("外观", false),
            ("快捷键", false),
        ];
        let sidebar = div()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(12.))
            .py(px(16.))
            .bg(hsla(0., 0., 0.14, 1.0))
            .child(
                div()
                    .px(px(8.))
                    .pb(px(12.))
                    .text_size(px(12.))
                    .text_color(NEUTRAL)
                    .child("设置"),
            )
            .children(sidebar_items.into_iter().map(|(label, active)| {
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .w_full()
                    .text_size(px(14.))
                    .bg(if active { hsla(0.55, 0.6, 0.45, 0.35) } else { hsla(0., 0., 0., 0.) })
                    .text_color(if active { hsla(0.55, 0.6, 0.85, 1.0) } else { NEUTRAL })
                    .child(label.to_string())
            }));

        let rows = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(providers.into_iter().map(|(id, count)| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(8.))
                    .bg(hsla(0., 0., 0.16, 1.0))
                    .child(div().text_size(px(14.)).text_color(hsla(0., 0., 0.85, 1.)).child(id))
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(3.))
                            .rounded(px(999.))
                            .bg(hsla(0.55, 0.5, 0.5, 0.25))
                            .text_size(px(12.))
                            .text_color(hsla(0.55, 0.5, 0.8, 1.0))
                            .child(format!("{count} 个模型")),
                    )
            }));

        let content = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(24.))
            .py(px(20.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(10.))
                    .child(div().text_xl().child("服务商"))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(NEUTRAL)
                            .child(format!("{agent} 会话可用的模型服务商")),
                    ),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(NEUTRAL)
                    .child(format!("共 {provider_count} 家 · {total_models} 个模型")),
            )
            .child(
                div()
                    .flex_1()
                    .id("provider-list")
                    .overflow_y_scroll()
                    .child(div().flex().flex_col().gap(px(6.)).child(rows)),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(hsla(0., 0., 0.85, 1.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(10.))
                    .child(div().text_size(px(13.)).text_color(hsla(0., 0., 0.9, 1.)).child("kumu"))
                    .child(div().flex_1().text_size(px(12.)).text_color(NEUTRAL).child(status)),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .child(sidebar)
                    .child(div().w_px().h_full().bg(hsla(0., 0., 0.22, 1.0)))
                    .child(content),
            )
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(980.), px(680.)), cx)),
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
