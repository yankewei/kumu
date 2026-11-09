//! Settings window, Waku-style: sidebar (back row + search + icon nav) on the
//! left; a centered content column with card-grouped rows on the right.

use std::collections::HashMap;

use super::widgets::{agent_logo, pill};
use crate::agent::{AGENTS, integration};
use crate::app::NotificationSettings;
use crate::db;
use crate::theme;
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;
use gpui_updater::{ErrorKind, UpdateStatus, Updater};

#[derive(Clone, Copy, PartialEq, Debug)]
enum SettingsPage {
    Providers,
    General,
}

/// One font picker entry: shown as `label`, identified by `family`.
#[derive(Clone)]
struct FontItem {
    label: SharedString,
    family: SharedString,
}

impl SearchableListItem for FontItem {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &SharedString {
        &self.family
    }

    /// Preview the family in its own typeface inside the dropdown.
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .font_family(self.family.clone())
            .child(self.label.clone())
    }
}

/// One UI font-size picker entry, in px.
#[derive(Clone)]
struct FontSizeItem(u32);

impl SearchableListItem for FontSizeItem {
    type Value = u32;

    fn title(&self) -> SharedString {
        self.0.to_string().into()
    }

    fn value(&self) -> &u32 {
        &self.0
    }
}

pub struct SettingsWindow {
    settings_page: SettingsPage,
    search_state: Entity<InputState>,
    content_scroll: ScrollHandle,
    /// UI font picker over the system's installed families.
    font_select: Entity<SelectState<SearchableVec<FontItem>>>,
    /// UI font-size picker over the app's type scale.
    font_size_select: Entity<SelectState<SearchableVec<FontSizeItem>>>,
    updater: Entity<Updater>,
    integrations: HashMap<&'static str, Result<integration::Status, String>>,
    integration_busy: Option<&'static str>,
    /// The window's overlay for hover labels; mounted by `window_layers`.
    tooltips: Entity<Tooltips>,
}

impl SettingsWindow {
    pub fn new(updater: Entity<Updater>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&updater, |_, updater, cx| {
            if let UpdateStatus::Errored(error) = updater.read(cx).status() {
                eprintln!("[updates] {error}");
            }
            cx.notify();
        })
        .detach();

        // Font picker: every installed family, preselecting the theme's
        // current one (None = placeholder if it's no longer installed).
        let font_select = cx.new(|cx| {
            let items = theme::system_fonts(cx)
                .into_iter()
                .map(|(label, family)| FontItem { label, family })
                .collect::<Vec<_>>();
            let current = cx.theme().font_family.to_string();
            let selected = items
                .iter()
                .position(|i| i.family.as_ref() == current.as_str())
                .map(IndexPath::new);
            SelectState::new(SearchableVec::from(items), selected, window, cx).searchable(true)
        });
        cx.subscribe_in(&font_select, window, |this, _, ev, window, cx| {
            this.on_font_selected(ev, window, cx);
        })
        .detach();

        // Font-size picker over the app's type scale, preselecting the size in
        // force — the theme's, which the persisted setting has already set.
        let font_size_select = cx.new(|cx| {
            let items = theme::FONT_SIZES
                .into_iter()
                .map(FontSizeItem)
                .collect::<Vec<_>>();
            let current = cx.theme().font_size;
            let selected = items
                .iter()
                .position(|item| px(item.0 as f32) == current)
                .map(IndexPath::new);
            SelectState::new(SearchableVec::from(items), selected, window, cx)
        });
        cx.subscribe_in(&font_size_select, window, |this, _, ev, window, cx| {
            this.on_font_size_selected(ev, window, cx);
        })
        .detach();

        Self {
            settings_page: SettingsPage::Providers,
            search_state: cx.new(|cx| InputState::new(window, cx).placeholder("搜索设置…")),
            content_scroll: ScrollHandle::new(),
            font_select,
            font_size_select,
            updater,
            integrations: AGENTS
                .iter()
                .map(|agent| {
                    (
                        agent.id,
                        integration::status(agent.id).map_err(|error| error.to_string()),
                    )
                })
                .collect(),
            integration_busy: None,
            tooltips: cx.new(|cx| Tooltips::new(cx)),
        }
    }

    fn change_integration(
        &mut self,
        agent: &'static str,
        remove: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.integration_busy.is_some() {
            return;
        }
        self.integration_busy = Some(agent);
        cx.notify();
        let window = window.window_handle();
        let work = cx.background_executor().spawn(async move {
            let result = if remove {
                integration::uninstall(agent)
            } else {
                integration::install(agent)
            };
            let status = integration::status(agent).map_err(|error| error.to_string());
            (result, status)
        });
        cx.spawn(async move |this, cx| {
            let (result, status) = work.await;
            let _ = this.update(cx, |this, cx| {
                this.integration_busy = None;
                this.integrations.insert(agent, status);
                let note = match result {
                    Ok(()) => Notification::success(if remove {
                        "已移除通知接入；请重新启动 Agent。"
                    } else if agent == "codex" {
                        "已注册通知 hooks；重新启动 Codex 后，请在 /hooks 中审查并信任。"
                    } else {
                        "已安装通知接入；请重新启动 Agent。"
                    }),
                    Err(error) => Notification::error(format!("通知接入失败：{error}")),
                };
                let _ = window.update(cx, |_, window, cx| window.push_notification(note, cx));
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the update card from the main window's persistent notice.
    /// A ready-to-restart notice also opens the existing confirmation dialog.
    pub fn show_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_page = SettingsPage::General;
        self.search_state
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.content_scroll.scroll_to_bottom();
        if matches!(self.updater.read(cx).status(), UpdateStatus::Staged(_)) {
            self.update_action(window, cx);
        }
        cx.notify();
    }

    /// Commit a font pick: persist the family and apply it app-wide.
    fn on_font_selected(
        &mut self,
        ev: &SelectEvent<SearchableVec<FontItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SelectEvent::Confirm(Some(family)) = ev else {
            return;
        };
        if cx.theme().font_family.as_ref() == family.as_ref() {
            return;
        }
        if let Err(error) = db::set_setting(db::SETTING_FONT, family) {
            window.push_notification(Notification::error(format!("保存字体失败：{error}")), cx);
        }
        // The choice still applies for this run; the notice says it will not
        // survive a restart.
        theme::set_font_family(cx, family);
    }

    /// Commit a font-size pick: persist the size and apply it app-wide, which
    /// hands every window a new rem size to lay out and draw against.
    fn on_font_size_selected(
        &mut self,
        ev: &SelectEvent<SearchableVec<FontSizeItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SelectEvent::Confirm(Some(size)) = ev else {
            return;
        };
        if px(*size as f32) == cx.theme().font_size {
            return;
        }
        if let Err(error) = db::set_setting(db::SETTING_FONT_SIZE, &size.to_string()) {
            window.push_notification(Notification::error(format!("保存字号失败：{error}")), cx);
        }
        theme::set_font_size(cx, *size as f32);
    }

    fn render_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
        let theme = cx.theme();
        let font_family = theme.font_family.clone();
        let (foreground, muted_fg) = (theme.foreground, theme.muted_foreground);
        let (sidebar, sidebar_fg, sidebar_border) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
        );
        let (sidebar_accent, sidebar_accent_fg) =
            (theme.sidebar_accent, theme.sidebar_accent_foreground);
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
                        this.content_scroll.set_offset(point(px(0.), px(0.)));
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
            SettingsPage::Providers => vec![self.render_providers_card(dark, theme, cx)],
            SettingsPage::General => vec![
                self.render_general_card(dark, theme, cx),
                self.render_updates_card(dark, theme, cx),
            ],
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
                    .track_scroll(&self.content_scroll)
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
            .child(overlays)
    }

    /// One card: elevated surface (muted bg) with hairline-separated rows.
    fn card(&self, bg: Hsla) -> Div {
        div().w_full().px_5().py_4().rounded_lg().bg(bg)
    }

    fn render_providers_card(&self, dark: bool, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let (base_bg, radius, primary, link, muted, muted_fg, border) = (
            theme.background,
            theme.radius,
            theme.primary,
            theme.link,
            theme.muted,
            theme.muted_foreground,
            theme.border,
        );
        // White card on the light panel; keep the muted surface in dark mode.
        let card_bg = if dark { muted } else { gpui_kit::gpui::white() };
        // Icon tiles flip to muted so they still contrast against the white card.
        let tile_bg = if dark { base_bg } else { muted };
        let rows = AGENTS
            .iter()
            .enumerate()
            .map(|(i, a)| {
                // The registry is what kumu supports; this is the one fact the
                // list takes from the machine. An agent that is not installed
                // is still shown, so its absence is visible.
                let installed = crate::agent::installed(a.id);
                let agent = a.id;
                let status = self.integrations.get(agent);
                let registered = matches!(status, Some(Ok(integration::Status::Registered)));
                let needs_repair = matches!(status, Some(Ok(integration::Status::NeedsRepair)));
                let notice = match status {
                    Some(Ok(integration::Status::Registered)) if agent == "codex" => {
                        "通知已注册；请在 Codex /hooks 中确认信任。"
                    }
                    Some(Ok(integration::Status::Registered)) => "通知已接入",
                    Some(Ok(integration::Status::NeedsRepair)) => "通知接入需要修复",
                    Some(Err(_)) => "无法读取通知接入状态",
                    _ => "通知未接入",
                };
                let status_pill = pill(
                    installed,
                    if installed { "已安装" } else { "未安装" },
                    radius,
                    primary,
                    link,
                    muted,
                    muted_fg,
                )
                .into_any_element();
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
                            .child(
                                div()
                                    .when(!installed, |d| d.opacity(0.45))
                                    .child(agent_logo(a.logo, px(16.), muted_fg)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_xs()
                                    .font_medium()
                                    .when(!installed, |d| d.text_color(muted_fg))
                                    .child(a.name),
                            )
                            .child(
                                div()
                                    .mt_0p5()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .text_ellipsis()
                                    .child(a.desc),
                            )
                            .child(div().mt_0p5().text_xs().text_color(muted_fg).child(notice)),
                    )
                    .child(
                        v_flex().items_end().gap_1().child(status_pill).child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new(format!("install-notifications-{agent}"))
                                        .small()
                                        .ghost()
                                        .label(if registered || needs_repair {
                                            "修复接入"
                                        } else {
                                            "接入通知"
                                        })
                                        .loading(self.integration_busy == Some(agent))
                                        .disabled(self.integration_busy.is_some())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.change_integration(agent, false, window, cx)
                                        })),
                                )
                                .when(registered || needs_repair, |row| {
                                    row.child(
                                        Button::new(format!("remove-notifications-{agent}"))
                                            .small()
                                            .ghost()
                                            .label("移除")
                                            .disabled(self.integration_busy.is_some())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.change_integration(agent, true, window, cx)
                                            })),
                                    )
                                }),
                        ),
                    )
            })
            .collect::<Vec<_>>();

        self.card(card_bg)
            .child(
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_xs().font_medium().child("编码 Agent"))
                    .child(div().text_xs().text_color(muted_fg).child(
                        "接入通知会注册到 Agent 的配置或扩展目录，之后可按原来的方式启动。",
                    )),
            )
            .child(v_flex().mt_1().children(rows))
            .into_any_element()
    }

    fn update_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.updater.read(cx).status().clone() {
            UpdateStatus::Available(_) => {
                self.updater
                    .update(cx, |updater, cx| updater.download_and_install(cx));
            }
            UpdateStatus::Downloading { .. } => {
                self.updater.update(cx, |updater, cx| updater.cancel(cx));
            }
            UpdateStatus::Staged(_) => {
                let updater = self.updater.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let updater = updater.clone();
                    alert
                        .title("重启以完成更新")
                        .description(
                            "重启将结束当前所有终端进程，请确认 Agent 任务已完成或已保存。",
                        )
                        .button_props(
                            DialogButtonProps::default()
                                .ok_text("重启更新")
                                .cancel_text("取消")
                                .show_cancel(true),
                        )
                        .on_ok(move |_, _, cx| {
                            updater.update(cx, |updater, cx| updater.restart(cx));
                            true
                        })
                });
            }
            UpdateStatus::Idle | UpdateStatus::UpToDate | UpdateStatus::Errored(_) => {
                self.updater.update(cx, |updater, cx| updater.check(cx));
            }
            _ => {}
        }
    }

    fn render_updates_card(&self, dark: bool, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let status = self.updater.read(cx).status();
        let (message, label) = match status {
            UpdateStatus::Idle => ("启动时自动检查更新。".to_string(), "检查更新"),
            UpdateStatus::Checking => ("正在检查新版本…".to_string(), "检查中"),
            UpdateStatus::UpToDate => ("当前已是最新版本。".to_string(), "检查更新"),
            UpdateStatus::Available(version) => (format!("发现新版本 {version}。"), "下载并安装"),
            UpdateStatus::Downloading { downloaded, total } => {
                let message = match total {
                    Some(total) if *total > 0 => {
                        format!("正在下载：{}%", downloaded.saturating_mul(100) / total)
                    }
                    _ => format!("正在下载：{:.1} MB", *downloaded as f64 / 1_048_576.),
                };
                (message, "取消下载")
            }
            UpdateStatus::Installing => ("正在验证并安装更新…".to_string(), "安装中"),
            UpdateStatus::Staged(version) => {
                (format!("版本 {version} 已安装，重启后生效。"), "重启更新")
            }
            UpdateStatus::Errored(error) => {
                let message = match error.kind() {
                    ErrorKind::Network => "连接更新服务失败，请稍后重试。",
                    ErrorKind::Metadata => "暂时没有适用于此设备的更新，请查看发布页面。",
                    ErrorKind::Verification => "更新校验未通过，请从发布页面下载安装。",
                    ErrorKind::Location => "请先将 Kumu 安装到应用程序目录，再进行更新。",
                    ErrorKind::Permission => "没有替换应用的权限，请手动下载安装。",
                    _ => "更新失败，请重试或从发布页面下载安装。",
                };
                (message.to_string(), "重试")
            }
            _ => ("暂时无法检查更新。".to_string(), "检查更新"),
        };
        let busy = matches!(status, UpdateStatus::Checking | UpdateStatus::Installing);
        self.card(if dark {
            theme.muted
        } else {
            gpui_kit::gpui::white()
        })
        .child(
            h_flex()
                .items_center()
                .gap_4()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_medium().child("软件更新"))
                        .child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("当前版本 {}", env!("CARGO_PKG_VERSION"))),
                        ),
                )
                .child(
                    Button::new("app-update")
                        .small()
                        .label(label)
                        .loading(busy)
                        .disabled(busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.update_action(window, cx)),
                        ),
                ),
        )
        .child(
            div()
                .mt_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(message),
        )
        .child(
            Button::new("release-page")
                .small()
                .ghost()
                .label("发布页面")
                .on_click(|_, _, cx| {
                    cx.open_url("https://github.com/yankewei/kumu/releases/latest")
                }),
        )
        .into_any_element()
    }

    fn render_general_card(&self, dark: bool, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let (radius, primary, link, muted, muted_fg, border) = (
            theme.radius,
            theme.primary,
            theme.link,
            theme.muted,
            theme.muted_foreground,
            theme.border,
        );
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
                        .child(div().mt_0p5().text_xs().text_color(muted_fg).child(desc)),
                )
                .child(control)
        };
        let switch = Switch::new("dark-mode")
            .checked(dark)
            .on_click(|checked, window, cx| {
                theme::change_mode(
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
        let notifications = Switch::new("terminal-notifications")
            .checked(cx.global::<NotificationSettings>().enabled)
            .on_click(|checked, window, cx| {
                if let Err(error) =
                    db::set_setting(db::SETTING_NOTIFICATIONS, if *checked { "1" } else { "0" })
                {
                    window.push_notification(
                        Notification::error(format!("保存通知设置失败：{error}")),
                        cx,
                    );
                    return;
                }
                cx.set_global(NotificationSettings { enabled: *checked });
                window.refresh();
            })
            .into_any_element();
        let font_select = div()
            .w_48()
            .child(
                Select::new(&self.font_select)
                    .small()
                    .placeholder("选择字体"),
            )
            .into_any_element();
        // Same control width as the family picker above, so the two typography
        // rows read as one column.
        let font_size_select = div()
            .w_48()
            .child(
                Select::new(&self.font_size_select)
                    .small()
                    .placeholder("选择字号"),
            )
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
                "界面字体",
                "选择应用的界面字体。",
                font_select,
                border,
                false,
            ))
            .child(row(
                "界面字体大小",
                "界面文字与间距的基础字号。",
                font_size_select,
                border,
                false,
            ))
            .child(row(
                "Agent 通知",
                "任务完成或需要输入、确认时提醒；应用在后台时发送系统通知。",
                notifications,
                border,
                false,
            ))
            .child(row(
                "本地数据",
                "项目与会话记录保存在 ~/.kumu/kumu.db。",
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
