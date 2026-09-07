//! Settings window: category nav on the left, setting rows on the right.
//! Opened from Home's gear icon; resizable with a minimum size it opens at.

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

        // ---- Left: search + category nav (Zed-style chevron rows). ----
        let nav_defs: [(&'static str, SettingsPage); 2] = [
            ("服务商", SettingsPage::Providers),
            ("通用", SettingsPage::General),
        ];
        let nav = nav_defs
            .into_iter()
            .filter(|(label, _)| query.is_empty() || label.to_lowercase().contains(&query))
            .map(|(label, page)| {
                let active = self.settings_page == page;
                div()
                    .id(SharedString::from(format!("settings-nav-{label}")))
                    .w_full()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded(radius)
                    .cursor_pointer()
                    .bg(if active { sidebar_accent } else { theme.transparent })
                    .text_color(if active { sidebar_accent_fg } else { sidebar_fg })
                    .when(!active, |this| {
                        this.hover(move |style| style.bg(sidebar_accent.opacity(0.5)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_page = page;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .small()
                            .text_color(if active { sidebar_accent_fg } else { muted_fg }),
                    )
                    .child(div().text_sm().child(label))
            })
            .collect::<Vec<_>>();

        let left = v_flex()
            .w_64()
            .h_full()
            .flex_shrink_0()
            .bg(sidebar)
            .text_color(sidebar_fg)
            .child(
                div().px_3().py_3().child(
                    Input::new(&self.search_state)
                        .small()
                        .prefix(Icon::new(IconName::Search).small().text_color(muted_fg)),
                ),
            )
            .child(v_flex().px_2().gap_0p5().children(nav))
            .child(div().flex_1())
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .text_xs()
                    .text_color(muted_fg)
                    .child("本地数据 · ~/.kumu/kumu.db"),
            );

        // ---- Right: page header + hairline-separated setting rows. ----
        let (title, desc) = match self.settings_page {
            SettingsPage::Providers => ("服务商", "通过 ACP 发现的本机 agent"),
            SettingsPage::General => ("通用", "界面外观与本地数据"),
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        let push_row = |rows: &mut Vec<AnyElement>, control: AnyElement, title: &'static str, desc: &'static str| {
            rows.push(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap_6()
                    .py_3p5()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_medium().child(title))
                            .child(div().text_xs().text_color(muted_fg).child(desc)),
                    )
                    .child(control)
                    .into_any_element(),
            );
        };
        let divider = |rows: &mut Vec<AnyElement>| {
            rows.push(div().h_px().w_full().bg(border).into_any_element());
        };

        match self.settings_page {
            SettingsPage::Providers => {
                for (i, a) in AGENTS.iter().enumerate() {
                    push_row(
                        &mut rows,
                        pill(true, "本地", radius, primary, link, muted, muted_fg).into_any_element(),
                        a.name,
                        a.desc,
                    );
                    if i + 1 < AGENTS.len() {
                        divider(&mut rows);
                    }
                }
            }
            SettingsPage::General => {
                push_row(
                    &mut rows,
                    Switch::new("dark-mode")
                        .checked(dark)
                        .on_click(|checked, window, cx| {
                            Theme::change(
                                if *checked { ThemeMode::Dark } else { ThemeMode::Light },
                                Some(window),
                                cx,
                            );
                        })
                        .into_any_element(),
                    "深色外观",
                    "在亮色与暗色主题之间切换。",
                );
                divider(&mut rows);
                push_row(
                    &mut rows,
                    pill(false, "SQLite", radius, primary, link, muted, muted_fg).into_any_element(),
                    "本地数据",
                    "会话与消息保存在 ~/.kumu/kumu.db。",
                );
            }
        }

        let right = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(
                v_flex()
                    .gap_1()
                    .px_8()
                    .pt_5()
                    .child(div().text_base().font_bold().child(title))
                    .child(div().text_sm().text_color(muted_fg).child(desc)),
            )
            .child(div().mt_4().h_px().w_full().bg(border))
            .child(
                div()
                    .flex_1()
                    .id("settings-content")
                    .overflow_y_scroll()
                    .px_8()
                    .pt_2()
                    .child(v_flex().max_w_128().children(rows)),
            );

        let title_bar = TitleBar::new()
            .child(div().text_sm().font_bold().child("kumu"))
            .child(div().text_sm().text_color(muted_fg).child("设置"));

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

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_settings(window, cx)
    }
}
