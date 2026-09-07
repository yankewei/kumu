//! Settings window, Waku-style: sidebar (back row + search + icon nav) on the
//! left; a centered content column with card-grouped rows on the right.

use super::widgets::pill;
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
}

impl SettingsWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            settings_page: SettingsPage::Providers,
            search_state: cx.new(|cx| InputState::new(window, cx).placeholder("搜索设置…")),
        }
    }

    fn render_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let font_family = theme.font_family.clone();
        let (foreground, muted_fg, border, radius) =
            (theme.foreground, theme.muted_foreground, theme.border, theme.radius);
        let (sidebar, sidebar_fg, sidebar_border) =
            (theme.sidebar, theme.sidebar_foreground, theme.sidebar_border);
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
            .text_sm()
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
                    .text_sm()
                    .text_color(if active { sidebar_accent_fg } else { sidebar_fg })
                    .bg(if active { sidebar_accent } else { theme.transparent })
                    .when(!active, |this| {
                        this.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_page = page;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(icon)
                            .with_size(px(15.))
                            .text_color(if active { sidebar_accent_fg } else { muted_fg }),
                    )
                    .child(label)
            })
            .collect::<Vec<_>>();

        let left = v_flex()
            .w_64()
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
                theme.background, radius, primary, link, muted, muted_fg, border,
            )],
            SettingsPage::General => vec![self.render_general_card(
                dark, radius, primary, link, muted, muted_fg, border,
            )],
        };

        let right = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
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
                                .child(div().text_lg().font_medium().child(title))
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
        base_bg: Hsla,
        radius: Pixels,
        primary: Hsla,
        link: Hsla,
        muted: Hsla,
        muted_fg: Hsla,
        border: Hsla,
    ) -> AnyElement {
        let rows = AGENTS
            .iter()
            .enumerate()
            .map(|(i, a)| {
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
                            .bg(base_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Bot)
                                    .with_size(px(16.))
                                    .text_color(muted_fg),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_medium().child(a.name))
                            .child(
                                div()
                                    .mt_0p5()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .text_ellipsis()
                                    .child(a.desc),
                            ),
                    )
                    .child(pill(true, "本地", radius, primary, link, muted, muted_fg))
            })
            .collect::<Vec<_>>();

        self.card(muted)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_medium().child("编码 Agent"))
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
                        .child(div().text_sm().font_medium().child(title))
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
                    if *checked { ThemeMode::Dark } else { ThemeMode::Light },
                    Some(window),
                    cx,
                );
            })
            .into_any_element();

        self.card(muted)
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
                pill(false, "SQLite", radius, primary, link, muted, muted_fg)
                    .into_any_element(),
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
