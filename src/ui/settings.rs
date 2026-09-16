//! Settings window, Waku-style: sidebar (back row + search + icon nav) on the
//! left; a centered content column with card-grouped rows on the right.

use std::collections::{HashMap, HashSet};

use futures::StreamExt;

use super::widgets::{agent_logo, pill};
use super::{AgentSessionOptions, SessionOptions};
use crate::agents::AGENTS;
use crate::backend;
use crate::db;
use crate::theme;
use crate::ui::layers;
use crate::ui::tooltip::Tooltips;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

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
    /// UI font picker over the system's installed families.
    font_select: Entity<SelectState<SearchableVec<FontItem>>>,
    /// UI font-size picker over the app's type scale.
    font_size_select: Entity<SelectState<SearchableVec<FontSizeItem>>>,
    /// Probed session options (models and modes) per agent id.
    session_options: HashMap<&'static str, SessionOptions>,
    /// Agents whose probe is still running.
    probing: HashSet<&'static str>,
    /// The window's overlay for hover labels; mounted by `window_layers`.
    tooltips: Entity<Tooltips>,
}

/// The registry agents this process has no cached options for, in registry order.
fn agents_missing_from(cached: &HashMap<&'static str, SessionOptions>) -> Vec<&'static str> {
    AGENTS
        .iter()
        .map(|agent| agent.id)
        .filter(|id| !cached.contains_key(id))
        .collect()
}

impl SettingsWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session_options = if cx.has_global::<AgentSessionOptions>() {
            cx.global::<AgentSessionOptions>().0.clone()
        } else {
            // Must exist up front: the probe handler below calls global_mut.
            cx.set_global(AgentSessionOptions::default());
            HashMap::new()
        };

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
        cx.subscribe_in(&font_select, window, |this, _, ev, _, cx| {
            this.on_font_selected(ev, cx);
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
        cx.subscribe_in(&font_size_select, window, |this, _, ev, _, cx| {
            this.on_font_size_selected(ev, cx);
        })
        .detach();

        // Ask for whatever this process has not discovered yet. The refresh
        // control on the providers card re-runs the same fan-out over every
        // agent.
        let missing = agents_missing_from(&session_options);
        let mut settings = Self {
            settings_page: SettingsPage::Providers,
            search_state: cx.new(|cx| InputState::new(window, cx).placeholder("搜索设置…")),
            font_select,
            font_size_select,
            session_options,
            probing: HashSet::new(),
            tooltips: cx.new(|cx| Tooltips::new(cx)),
        };
        settings.probe(missing, cx);
        settings
    }

    /// Ask each agent for its session options, replacing whatever is cached as
    /// the answers arrive.
    ///
    /// This is what the composer's pickers read, and so the only way to refresh
    /// a catalogue mid-run: an agent that was updated, restarted, or pointed at
    /// another account is otherwise cached for the life of the process.
    fn probe(&mut self, agents: Vec<&'static str>, cx: &mut Context<Self>) {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<backend::Event>();
        for agent in agents {
            if let Some(cfg) = backend::Config::for_agent(agent) {
                backend::probe(agent, cfg, ev_tx.clone());
                self.probing.insert(agent);
            }
        }
        drop(ev_tx);

        // Bridge the agent threads' std channel into the UI: a plain forwarder
        // thread feeds an async channel the main thread can await.
        let (bridge_tx, mut bridge_rx) = futures::channel::mpsc::unbounded::<backend::Event>();
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
                    backend::Event::SessionOptions {
                        agent,
                        models,
                        current_model,
                        modes,
                        current_mode,
                        access,
                        current_access,
                        sandboxes,
                        current_sandbox,
                        reasoning,
                        current_reasoning,
                    } => Some((
                        *agent,
                        Some(SessionOptions {
                            models: models.clone(),
                            current_model: current_model.clone(),
                            modes: modes.clone(),
                            current_mode: current_mode.clone(),
                            access: access.clone(),
                            current_access: current_access.clone(),
                            sandboxes: sandboxes.clone(),
                            current_sandbox: current_sandbox.clone(),
                            reasoning: reasoning.clone(),
                            current_reasoning: current_reasoning.clone(),
                        }),
                    )),
                    backend::Event::ProbeFailed { agent } => Some((*agent, None)),
                    _ => None,
                };
                let Some((agent, options)) = outcome else {
                    continue;
                };
                let _ = this.update(cx, |settings, cx| {
                    settings.probing.remove(&agent);
                    if let Some(options) = options {
                        settings.session_options.insert(agent, options.clone());
                        cx.global_mut::<AgentSessionOptions>()
                            .0
                            .insert(agent, options);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Commit a font pick: persist the family and apply it app-wide.
    fn on_font_selected(
        &mut self,
        ev: &SelectEvent<SearchableVec<FontItem>>,
        cx: &mut Context<Self>,
    ) {
        let SelectEvent::Confirm(Some(family)) = ev else {
            return;
        };
        if cx.theme().font_family.as_ref() == family.as_ref() {
            return;
        }
        db::set_setting(db::SETTING_FONT, family).ok();
        theme::set_font_family(cx, family);
    }

    /// Commit a font-size pick: persist the size and apply it app-wide, which
    /// hands every window a new rem size to lay out and draw against.
    fn on_font_size_selected(
        &mut self,
        ev: &SelectEvent<SearchableVec<FontSizeItem>>,
        cx: &mut Context<Self>,
    ) {
        let SelectEvent::Confirm(Some(size)) = ev else {
            return;
        };
        if px(*size as f32) == cx.theme().font_size {
            return;
        }
        db::set_setting(db::SETTING_FONT_SIZE, &size.to_string()).ok();
        theme::set_font_size(cx, *size as f32);
    }

    fn render_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let overlays = layers::window_layers(&self.tooltips, window, cx);
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
                cx,
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
            .child(overlays)
    }

    /// One card: elevated surface (muted bg) with hairline-separated rows.
    fn card(&self, bg: Hsla) -> Div {
        div().w_full().px_5().py_4().rounded_lg().bg(bg)
    }

    fn render_providers_card(
        &self,
        cx: &Context<Self>,
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
                let status_pill = if let Some(catalog) = self.session_options.get(a.id) {
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
                h_flex()
                    .items_start()
                    .gap_6()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_xs().font_medium().child("编码 Agent"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child("探测本机 agent 的模型与可用选项"),
                            ),
                    )
                    // Plain text, like every other action here: the agent's own
                    // probe is what fills the model pickers, so re-running it is
                    // how a catalogue changed on this machine is picked up —
                    // otherwise it stays cached for the life of the process. The
                    // rows' own pills show which agents are being asked.
                    .child(
                        div()
                            .id("refresh-agents")
                            .flex_none()
                            .cursor_pointer()
                            .px_1p5()
                            .py_0p5()
                            .rounded(radius)
                            .text_xs()
                            .text_color(link)
                            .hover(move |style| style.bg(muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let all = AGENTS.iter().map(|agent| agent.id).collect();
                                this.probe(all, cx);
                                cx.notify();
                            }))
                            .child("刷新"),
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

#[cfg(test)]
mod tests {
    // `use gpui_kit::*` in the parent brings gpui's own `test` attribute into
    // scope through the glob, so the built-in one is imported explicitly.
    use core::prelude::v1::test;

    use super::{AGENTS, agents_missing_from};
    use crate::ui::SessionOptions;
    use std::collections::HashMap;

    /// Startup asks only for what this process has not discovered, so opening
    /// settings does not re-spawn every agent; the card's refresh control is what
    /// asks for all of them.
    #[test]
    fn startup_probes_only_the_agents_with_no_cached_options() {
        let mut cached = HashMap::new();
        cached.insert("codex", SessionOptions::default());

        let missing = agents_missing_from(&cached);
        assert!(!missing.contains(&"codex"), "a cached agent is left alone");
        assert_eq!(
            missing.len(),
            AGENTS.len() - 1,
            "every other registry agent is asked for"
        );
        assert_eq!(agents_missing_from(&HashMap::new()).len(), AGENTS.len());
    }
}
