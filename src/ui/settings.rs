//! Settings window, Waku-style: sidebar (back row + search + icon nav) on the
//! left; a centered content column with card-grouped rows on the right.

use std::collections::{HashMap, HashSet};

use futures::StreamExt;

use super::widgets::{agent_logo, pill};
use super::{AgentModels, ModelCatalog};
use crate::acp;
use crate::agents::AGENTS;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

#[derive(Clone, Copy, PartialEq, Debug)]
enum SettingsPage {
    Providers,
    General,
}

pub struct SettingsWindow {
    settings_page: SettingsPage,
    search_state: Entity<InputState>,
    /// Probed model catalogs per agent id.
    models: HashMap<&'static str, ModelCatalog>,
    /// Agents whose probe is still running.
    probing: HashSet<&'static str>,
}

impl SettingsWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let models = if cx.has_global::<AgentModels>() {
            cx.global::<AgentModels>().0.clone()
        } else {
            // Must exist up front: the probe handler below calls global_mut.
            cx.set_global(AgentModels::default());
            HashMap::new()
        };

        // Probe every not-yet-cached agent over ACP for its model list.
        let mut probing = HashSet::new();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<acp::Event>();
        for a in AGENTS.iter() {
            if models.contains_key(a.id) {
                continue;
            }
            if let Some(cfg) = acp::Config::for_agent(a.id) {
                acp::probe(a.id, cfg, ev_tx.clone());
                probing.insert(a.id);
            }
        }
        drop(ev_tx);

        // Bridge the ACP threads' std channel into the UI: a plain forwarder
        // thread feeds an async channel the main thread can await.
        let (bridge_tx, mut bridge_rx) = futures::channel::mpsc::unbounded::<acp::Event>();
        std::thread::spawn(move || {
            for ev in ev_rx {
                if bridge_tx.unbounded_send(ev).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(ev) = bridge_rx.next().await {
                let outcome = match &ev {
                    acp::Event::Models {
                        agent,
                        models,
                        current,
                    } => Some((
                        *agent,
                        Some(ModelCatalog {
                            models: models.clone(),
                            current: current.clone(),
                        }),
                    )),
                    acp::Event::ProbeFailed { agent } => Some((*agent, None)),
                    _ => None,
                };
                let Some((agent, catalog)) = outcome else {
                    continue;
                };
                let _ = this.update(cx, |settings, cx| {
                    settings.probing.remove(&agent);
                    if let Some(catalog) = catalog {
                        settings.models.insert(agent, catalog.clone());
                        cx.global_mut::<AgentModels>().0.insert(agent, catalog);
                    }
                    cx.notify();
                });
            }
        })
        .detach();

        Self {
            settings_page: SettingsPage::Providers,
            search_state: cx.new(|cx| InputState::new(window, cx).placeholder("搜索设置…")),
            models,
            probing,
        }
    }

    fn render_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let font_family = theme.font_family.clone();
        let (foreground, muted_fg, border, radius) = (
            theme.foreground,
            theme.muted_foreground,
            theme.border,
            theme.radius,
        );
        let (sidebar, sidebar_fg, sidebar_border) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
        );
        let (sidebar_accent, sidebar_accent_fg) =
            (theme.sidebar_accent, theme.sidebar_accent_foreground);
        let (primary, link, muted) = (theme.primary, theme.link, theme.muted);
        let dark = theme.mode.is_dark();

        let query = self.search_state.read(cx).value().to_lowercase();

        // ---- Left: back row + search + icon nav (Waku-style sidebar). ----
        let back = div()
            .id("settings-back")
            .h_9()
            .px_2p5()
            .rounded_lg()
            .flex()
            .items_center()
            .gap_2p5()
            .cursor_pointer()
            .text_xs()
            .text_color(sidebar_fg)
            .hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
            .on_click(|_, window, _| window.remove_window())
            .child(Icon::new(IconName::ArrowLeft).small().text_color(muted_fg))
            .child("返回");

        let nav_defs: [(IconName, &'static str, SettingsPage); 2] = [
            (IconName::Bot, "服务商", SettingsPage::Providers),
            (IconName::Settings, "通用", SettingsPage::General),
        ];
        let nav = nav_defs
            .into_iter()
            .filter(|(_, label, _)| query.is_empty() || label.to_lowercase().contains(&query))
            .map(|(icon, label, page)| {
                let active = self.settings_page == page;
                div()
                    .id(SharedString::from(format!("settings-nav-{label}")))
                    .h_9()
                    .px_2p5()
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(if active {
                        sidebar_accent_fg
                    } else {
                        sidebar_fg
                    })
                    .bg(if active {
                        sidebar_accent
                    } else {
                        theme.transparent
                    })
                    .when(!active, |this| {
                        this.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_page = page;
                        cx.notify();
                    }))
                    .child(Icon::new(icon).with_size(px(15.)).text_color(if active {
                        sidebar_accent_fg
                    } else {
                        muted_fg
                    }))
                    .child(label)
            })
            .collect::<Vec<_>>();

        let left = v_flex()
            .w_56()
            .h_full()
            .flex_shrink_0()
            .bg(sidebar)
            .text_color(sidebar_fg)
            .child(div().px_2().pt_3().child(back))
            .child(
                div().px_2().pt_3().child(
                    Input::new(&self.search_state)
                        .small()
                        .prefix(Icon::new(IconName::Search).small().text_color(muted_fg)),
                ),
            )
            .child(div().h_2())
            .child(v_flex().px_2().gap_0p5().children(nav));

        // ---- Right: centered column with the page title + grouped cards. ----
        let title = match self.settings_page {
            SettingsPage::Providers => "服务商",
            SettingsPage::General => "通用",
        };

        let cards: Vec<AnyElement> = match self.settings_page {
            SettingsPage::Providers => vec![self.render_providers_card(
                dark,
                theme.background,
                radius,
                primary,
                link,
                muted,
                muted_fg,
                border,
            )],
            SettingsPage::General => {
                vec![self.render_general_card(dark, radius, primary, link, muted, muted_fg, border)]
            }
        };

        let right = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(if dark {
                theme.background
            } else {
                gpui_kit::gpui::white()
            })
            .child(
                div()
                    .flex_1()
                    .id("settings-content")
                    .overflow_y_scroll()
                    .px_8()
                    .pt_4()
                    .pb_8()
                    .child(
                        v_flex().max_w(rems(40.)).mx_auto().child(
                            v_flex()
                                .gap_5()
                                .child(div().text_sm().font_medium().child(title))
                                .children(cards),
                        ),
                    ),
            );

        let title_bar = TitleBar::new().child(div().text_sm().font_bold().child("kumu"));

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
                    .child(div().w_px().h_full().bg(sidebar_border))
                    .child(right),
            )
    }

    /// One card: elevated surface (muted bg) with hairline-separated rows.
    fn card(&self, bg: Hsla) -> Div {
        div().w_full().px_5().py_4().rounded_lg().bg(bg)
    }

    fn render_providers_card(
        &self,
        dark: bool,
        base_bg: Hsla,
        radius: Pixels,
        primary: Hsla,
        link: Hsla,
        muted: Hsla,
        muted_fg: Hsla,
        border: Hsla,
    ) -> AnyElement {
        // White card on the light panel; keep the muted surface in dark mode.
        let card_bg = if dark { muted } else { gpui_kit::gpui::white() };
        // Icon tiles flip to muted so they still contrast against the white card.
        let tile_bg = if dark { base_bg } else { muted };
        let rows = AGENTS
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let status_pill = if let Some(catalog) = self.models.get(a.id) {
                    let providers = catalog
                        .models
                        .iter()
                        .map(|m| m.provider.as_str())
                        .collect::<HashSet<_>>()
                        .len();
                    pill(
                        false,
                        format!("{} 模型 · {} 服务商", catalog.models.len(), providers),
                        radius,
                        primary,
                        link,
                        muted,
                        muted_fg,
                    )
                    .into_any_element()
                } else if self.probing.contains(&a.id) {
                    pill(false, "检测模型…", radius, primary, link, muted, muted_fg)
                        .into_any_element()
                } else {
                    pill(false, "模型未知", radius, primary, link, muted, muted_fg)
                        .into_any_element()
                };
                h_flex()
                    .items_center()
                    .gap_3()
                    .py_3()
                    .when(i + 1 < AGENTS.len(), |row| {
                        row.border_b_1().border_color(border)
                    })
                    // 30px icon tile on the card's lighter base.
                    .child(
                        div()
                            .size(px(30.))
                            .flex_none()
                            .rounded(radius)
                            .bg(tile_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(agent_logo(a.logo, px(16.), muted_fg)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_xs().font_medium().child(a.name))
                            .child(
                                div()
                                    .mt_0p5()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .text_ellipsis()
                                    .child(a.desc),
                            ),
                    )
                    .child(status_pill)
            })
            .collect::<Vec<_>>();

        self.card(card_bg)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().font_medium().child("编码 Agent"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("通过 ACP 发现的本机 agent"),
                    ),
            )
            .child(v_flex().mt_1().children(rows))
            .into_any_element()
    }

    fn render_general_card(
        &self,
        dark: bool,
        radius: Pixels,
        primary: Hsla,
        link: Hsla,
        muted: Hsla,
        muted_fg: Hsla,
        border: Hsla,
    ) -> AnyElement {
        let row = |title: &'static str,
                   desc: &'static str,
                   control: AnyElement,
                   border: Hsla,
                   last: bool| {
            h_flex()
                .items_center()
                .gap_6()
                .py_3()
                .when(!last, |r| r.border_b_1().border_color(border))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_xs().font_medium().child(title))
                        .child(
                            div()
                                .mt_0p5()
                                .text_xs()
                                .text_color(muted_fg)
                                .child(desc),
                        ),
                )
                .child(control)
        };
        let switch = Switch::new("dark-mode")
            .checked(dark)
            .on_click(|checked, window, cx| {
                Theme::change(
                    if *checked {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                    Some(window),
                    cx,
                );
            })
            .into_any_element();

        self.card(if dark { muted } else { gpui_kit::gpui::white() })
            .child(row(
                "深色外观",
                "在亮色与暗色主题之间切换。",
                switch,
                border,
                false,
            ))
            .child(row(
                "本地数据",
                "会话与消息保存在 ~/.kumu/kumu.db。",
                pill(false, "SQLite", radius, primary, link, muted, muted_fg).into_any_element(),
                border,
                true,
            ))
            .into_any_element()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_settings(window, cx)
    }
}
