//! kumu: settings page (Providers) — lists the model service providers a local
//! ACP agent can use, derived from the agent's session `configOptions`
//! (category: model). Rendered with gpui-component's themed Sidebar + tokens;
//! no hard-coded colors.
//!
//! Scope: single agent (default omp, `KUMU_AGENT=opencode` to switch), one
//! session, read-only (no credential editing).

mod acp;

use std::sync::mpsc;
use std::time::Duration;

use gpui_kit::component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use tokio::sync::mpsc::UnboundedSender;

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
            acp::Event::Agent(_) | acp::Event::Done(_) | acp::Event::Permission(_) => {}
        }
        cx.notify();
    }
}

impl Render for KumuApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let status = self.status.clone();
        let providers = self.providers.clone();
        let provider_count = providers.len();
        let providers_empty = providers.is_empty();
        let total_models = self.total_models;
        let agent = std::env::var("KUMU_AGENT").unwrap_or_else(|_| "omp".into());

        let sidebar = Sidebar::new("kumu-sidebar")
            .w(px(236.))
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .size_6()
                                .rounded(theme.radius)
                                .bg(theme.primary)
                                .text_color(theme.primary_foreground)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .font_bold()
                                .child("k"),
                        )
                        .child(div().font_bold().child("kumu")),
                ),
            )
            .child(
                SidebarGroup::new("设置").child(
                    SidebarMenu::new().children([
                        SidebarMenuItem::new("服务商").active(true),
                        SidebarMenuItem::new("会话"),
                        SidebarMenuItem::new("外观"),
                    ]),
                ),
            )
            .footer(
                SidebarFooter::new().child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .text_color(theme.muted_foreground)
                        .text_xs()
                        .child(div().size_2().rounded_full().bg(theme.primary))
                        .child(format!("{agent} · {status}")),
                ),
            );

        // Provider rows as bordered surface cards.
        let rows = v_flex()
            .gap_2()
            .children(providers.into_iter().map(|(id, count)| {
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .child(div().font_medium().text_sm().child(id)),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .px_2()
                                    .py(px(2.))
                                    .rounded(px(999.))
                                    .bg(theme.primary)
                                    .text_color(theme.primary_foreground)
                                    .text_size(px(11.))
                                    .child(count.to_string()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("个模型"),
                            ),
                    )
            }));

        let content = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .px_5()
                    .pt_5()
                    .child(div().text_lg().font_bold().child("服务商"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!("{agent} 会话可用的模型服务商")),
                    ),
            )
            .child(
                div()
                    .px_5()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("共 {provider_count} 家 · {total_models} 个模型")),
            )
            .child(
                div()
                    .flex_1()
                    .id("provider-list")
                    .overflow_y_scroll()
                    .px_5()
                    .py_3()
                    .child(
                        rows.when(providers_empty, |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("等待 agent 返回模型列表…"),
                            )
                        }),
                    ),
            );

        h_flex()
            .size_full()
            .bg(theme.background)
            .child(sidebar)
            .child(div().w_px().h_full().bg(theme.border))
            .child(content)
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(980.), px(640.)), cx)),
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
