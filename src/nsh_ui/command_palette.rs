//! Pure Rust NSH `⌘K` Quick Switcher & Command Palette overlay.
//!
//! Mounted ONLY while open (transparent full-window overlay over `KumuApp`),
//! so closed palettes carry zero runtime cost and never interfere with live
//! terminals.

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::mount::MountRegistry;
use super::theme::{appearance, nsh_theme};
use crate::agent::AgentState;
use crate::app::layout::PaneId;
use crate::theme::KumuTheme;

/// Logical group shown in the command palette when browsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteSection {
    Agents,
    Projects,
    Tabs,
    Actions,
}

impl PaletteSection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agents => "活动 Agent",
            Self::Projects => "项目",
            Self::Tabs => "当前项目页签",
            Self::Actions => "快捷操作",
        }
    }
}

/// Icon displayed on the left of a palette row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteIcon {
    Svg(&'static str),
    AgentLogo(Option<&'static str>),
}

/// Concrete host action triggered when a palette item is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteTarget {
    RevealAgent(PaneId),
    SelectProject(i64),
    SelectTab(usize),
    NewTab,
    SplitRight,
    SplitDown,
    ToggleZoomPane,
    ClosePane,
    NewAgentPane(String),
    TogglePlugin(&'static str),
    ToggleSidebar,
    ToggleTheme,
    InstallCli,
    OpenSettings,
    OpenOnboarding,
}

/// One selectable entry in the command palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub section: PaletteSection,
    pub title: String,
    pub subtitle: Option<String>,
    pub keywords: String,
    pub icon: PaletteIcon,
    pub agent_state: Option<AgentState>,
    pub badge: Option<String>,
    pub shortcut: Option<&'static str>,
    pub target: PaletteTarget,
}

/// Outcome emitted when the palette closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteOutcome {
    Selected(PaletteTarget),
    Dismissed,
}

/// Case-insensitive multi-term matcher returning item indices ordered by
/// match quality (exact prefix > substring > subsequence), preserving original
/// section order when `query` is empty.
pub fn filter_items(items: &[PaletteItem], query: &str) -> Vec<usize> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return (0..items.len()).collect();
    }

    let mut scored: Vec<(i32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let title = item.title.to_lowercase();
            let subtitle = item
                .subtitle
                .as_deref()
                .map(str::to_lowercase)
                .unwrap_or_default();
            let keywords = item.keywords.to_lowercase();
            let section = item.section.label().to_lowercase();

            let mut total = 0i32;
            for term in &terms {
                let best = [
                    score_field(&title, term, 120),
                    score_field(&subtitle, term, 60),
                    score_field(&keywords, term, 50),
                    score_field(&section, term, 30),
                ]
                .into_iter()
                .flatten()
                .max()?;
                total += best;
            }
            Some((total, index))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, index)| index).collect()
}

fn score_field(haystack: &str, needle: &str, base: i32) -> Option<i32> {
    if haystack.is_empty() {
        return None;
    }
    if haystack == needle {
        return Some(base + 80);
    }
    if haystack.starts_with(needle) {
        return Some(base + 50);
    }
    if let Some(pos) = haystack.find(needle) {
        let penalty = (pos as i32).min(24);
        return Some(base + 30 - penalty);
    }
    subsequence_score(haystack, needle).map(|bonus| base / 2 + bonus)
}

fn subsequence_score(haystack: &str, needle: &str) -> Option<i32> {
    let mut needle_chars = needle.chars();
    let mut current = needle_chars.next()?;
    let mut consecutive = 0i32;
    let mut bonus = 0i32;
    let mut prev_matched = false;

    for ch in haystack.chars() {
        if ch == current {
            if prev_matched {
                consecutive += 1;
                bonus += 4 + consecutive.min(6);
            } else {
                consecutive = 0;
                bonus += 1;
            }
            prev_matched = true;
            match needle_chars.next() {
                Some(next) => current = next,
                None => return Some(bonus.min(25)),
            }
        } else {
            prev_matched = false;
        }
    }
    None
}

struct PaletteMount {
    items: Vec<PaletteItem>,
    outcome_tx: UnboundedSender<PaletteOutcome>,
}

static MOUNTS: MountRegistry<PaletteMount> = MountRegistry::new();

mod view {
    use std::rc::Rc;

    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::{input, theme as active_theme};
    use nsh_core::element::{AccessibilityRole, scroll};
    use nsh_core::prelude::*;

    use super::{
        MOUNTS, PaletteIcon, PaletteItem, PaletteOutcome, PaletteSection, PaletteTarget,
        filter_items,
    };
    use crate::agent::AgentState;
    use crate::nsh_ui::assets::{agent_logo, icon_svg};
    use crate::nsh_ui::components::meta_text;
    use crate::nsh_ui::theme::{
        TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_sm_size, text_xs_size,
    };

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct PaletteRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn link_color() -> AdaptiveColor {
        kumu_color(TOKEN_LINK, |t| t.primary)
    }

    fn warning_color() -> AdaptiveColor {
        kumu_color(TOKEN_WARNING, |t| t.destructive)
    }

    fn success_color() -> AdaptiveColor {
        kumu_color(TOKEN_SUCCESS, |t| t.primary)
    }

    fn render_state_pill(state: AgentState) -> AnyElement {
        let color = move || match state {
            AgentState::Running => link_color(),
            AgentState::Waiting => warning_color(),
            AgentState::Completed => success_color(),
            AgentState::Failed => active_theme().destructive,
        };
        let glyph: AnyElement = match state {
            AgentState::Running => icon_svg("icons/loader-circle.svg", 11.0)
                .app_class("animate-spin")
                .text_color(color)
                .into_element(),
            AgentState::Waiting => view()
                .w(px(6.0))
                .h(px(6.0))
                .rounded(px(3.0))
                .bg(color)
                .into_element(),
            AgentState::Completed => icon_svg("icons/check.svg", 11.0)
                .text_color(color)
                .into_element(),
            AgentState::Failed => icon_svg("icons/triangle-alert.svg", 11.0)
                .text_color(color)
                .into_element(),
        };
        view()
            .flex_row()
            .flex_shrink_0()
            .h(px(18.0))
            .gap_1()
            .px_1p5()
            .items_center()
            .rounded(move || active_theme().radius)
            .bg(move || color().opacity(0.14))
            .child(glyph)
            .child(
                text(state.label())
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color),
            )
            .into_element()
    }

    fn render_item_icon(icon: PaletteIcon) -> AnyElement {
        match icon {
            PaletteIcon::Svg(path) => icon_svg(path, 14.0)
                .text_color(move || active_theme().muted_foreground)
                .into_element(),
            PaletteIcon::AgentLogo(logo) => agent_logo(logo, 14.0),
        }
    }

    #[component]
    pub(super) fn PaletteRoot(props: PaletteRootProps) -> impl IntoElement {
        let mount = MOUNTS.take(props.mount_id.get_untracked());
        let (items, tx) = match mount {
            Some(m) => (Rc::new(m.items), Some(m.outcome_tx)),
            None => (Rc::new(Vec::<PaletteItem>::new()), None),
        };

        let query = signal(String::new());
        let selected_pos = signal(0usize);
        let dispatched = stored(false);

        let emit_once = move |tx: &Option<UnboundedSender<PaletteOutcome>>,
                              outcome: PaletteOutcome| {
            let should_send = dispatched.with(|done| {
                if *done {
                    false
                } else {
                    *done = true;
                    true
                }
            });
            if should_send && let Some(tx) = tx {
                let _ = tx.unbounded_send(outcome);
            }
        };

        let items_enter = items.clone();
        let tx_enter = tx.clone();
        let submit_selected = Rc::new(move || {
            let indices = filter_items(&items_enter, &query.get_untracked());
            let pos = selected_pos.get_untracked();
            if let Some(&idx) = indices.get(pos)
                && let Some(item) = items_enter.get(idx)
            {
                emit_once(&tx_enter, PaletteOutcome::Selected(item.target.clone()));
            }
        });

        let items_key = items.clone();
        let tx_key = tx.clone();
        let submit_key = submit_selected.clone();
        let on_root_key = move |ev: &nsh_core::KeyEvent| {
            if ev.key == "Escape" || (ev.modifiers.cmd && ev.key.eq_ignore_ascii_case("k")) {
                emit_once(&tx_key, PaletteOutcome::Dismissed);
                return;
            }
            let len = filter_items(&items_key, &query.get_untracked()).len();
            if len == 0 {
                return;
            }
            if ev.key == "ArrowDown" || (ev.modifiers.ctrl && ev.key.eq_ignore_ascii_case("n")) {
                selected_pos.update(|pos| *pos = (*pos + 1) % len);
            } else if ev.key == "ArrowUp" || (ev.modifiers.ctrl && ev.key.eq_ignore_ascii_case("p"))
            {
                selected_pos.update(|pos| *pos = (*pos + len - 1) % len);
            } else if ev.key == "Enter" {
                submit_key();
            }
        };

        let items_list = items.clone();
        let tx_list = tx.clone();
        let results_list = dynamic(move || {
            let current_query = query.get();
            let indices = filter_items(&items_list, &current_query);
            let show_sections = current_query.trim().is_empty();
            if indices.is_empty() {
                return view()
                    .w_full()
                    .py_8()
                    .items_center()
                    .justify_center()
                    .child(meta_text("没有匹配的结果"))
                    .into_element();
            }

            let mut col = view().flex_col().w_full().p_1p5().gap_0p5();
            let mut last_section: Option<PaletteSection> = None;

            for (row_pos, idx) in indices.into_iter().enumerate() {
                let item = &items_list[idx];
                if show_sections && last_section != Some(item.section) {
                    last_section = Some(item.section);
                    let mut header = view().w_full().px_2().pb_0p5();
                    if row_pos > 0 {
                        header = header.pt_2();
                    } else {
                        header = header.pt_0p5();
                    }
                    col = col.child(
                        header.child(
                            text(item.section.label())
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || active_theme().muted_foreground),
                        ),
                    );
                }

                let target: PaletteTarget = item.target.clone();
                let tx_row = tx_list.clone();
                let title = item.title.clone();
                let subtitle = item.subtitle.clone();
                let icon = item.icon;
                let agent_state = item.agent_state;
                let badge = item.badge.clone();
                let shortcut = item.shortcut;

                let mut row = view()
                    .role(AccessibilityRole::Button)
                    .label(title.clone())
                    .flex_row()
                    .w_full()
                    .h(px(34.0))
                    .px_2p5()
                    .gap_2()
                    .items_center()
                    .rounded(move || active_theme().radius)
                    .app_class("cursor-pointer")
                    .bg(move || {
                        if selected_pos.get() == row_pos {
                            active_theme().accent
                        } else {
                            AdaptiveColor::TRANSPARENT
                        }
                    })
                    .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                    .on_press(move || {
                        emit_once(&tx_row, PaletteOutcome::Selected(target.clone()));
                    })
                    .child(render_item_icon(icon))
                    .child(
                        text(title)
                            .flex_shrink_0()
                            .max_w(px(240.0))
                            .min_w_0()
                            .app_class("truncate")
                            .text_size(text_xs_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(move || active_theme().foreground),
                    );

                if let Some(sub) = subtitle {
                    row = row.child(
                        text(sub)
                            .flex_1()
                            .min_w_0()
                            .app_class("truncate")
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().muted_foreground),
                    );
                } else {
                    row = row.child(view().flex_1());
                }

                if let Some(state) = agent_state {
                    row = row.child(render_state_pill(state));
                }
                if let Some(badge) = badge {
                    row = row.child(
                        view()
                            .flex_shrink_0()
                            .px_1p5()
                            .py_0p5()
                            .rounded(move || active_theme().radius)
                            .bg(move || active_theme().muted)
                            .child(
                                text(badge)
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    );
                }
                if let Some(shortcut) = shortcut {
                    row = row.child(
                        view()
                            .flex_shrink_0()
                            .px_1p5()
                            .py_0p5()
                            .rounded(move || active_theme().radius)
                            .border_1()
                            .border_color(move || active_theme().border)
                            .bg(move || active_theme().background)
                            .child(
                                text(shortcut)
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    );
                }

                col = col.child(row);
            }

            col.into_element()
        });

        let submit_input = submit_selected.clone();
        let tx_backdrop = tx.clone();

        let card = view()
            .flex_col()
            .w(px(540.0))
            .app_class("max-w-[92%]")
            .rounded(px(10.0))
            .bg(move || active_theme().popover)
            .border_1()
            .border_color(move || active_theme().border)
            .shadow_lg()
            .overflow_hidden()
            .child(
                view()
                    .flex_row()
                    .w_full()
                    .px_3()
                    .py_2p5()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_b_color(move || active_theme().border)
                    .child(
                        icon_svg("icons/search.svg", 14.0)
                            .text_color(move || active_theme().muted_foreground),
                    )
                    .child(
                        input()
                            .placeholder("搜索 Agent、项目、页签或快捷操作…")
                            .placeholder_color(move || active_theme().muted_foreground.light)
                            .auto_focus(true)
                            .flex_1()
                            .min_w_0()
                            .text_size(text_sm_size)
                            .text_color(move || active_theme().foreground)
                            .value(move || query.get())
                            .on_change(move |v: &str| {
                                query.set(v.to_owned());
                                selected_pos.set(0);
                            })
                            .on_submit(move |_: &str| submit_input()),
                    )
                    .child(
                        view()
                            .flex_shrink_0()
                            .px_1p5()
                            .py_0p5()
                            .rounded(move || active_theme().radius)
                            .bg(move || active_theme().muted)
                            .child(
                                text("Esc")
                                    .text_size(text_xs_size)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    ),
            )
            .child(scroll().w_full().max_h(px(380.0)).child(results_list))
            .child(
                view()
                    .flex_row()
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .items_center()
                    .border_t_1()
                    .border_t_color(move || active_theme().border)
                    .bg(move || active_theme().muted.opacity(0.35))
                    .child(meta_text("↑↓ 选择"))
                    .child(meta_text("↩ 执行"))
                    .child(meta_text("Esc 关闭")),
            );

        view()
            .relative()
            .flex_1()
            .w_full()
            .h_full()
            .pt(px(72.0))
            .items_center()
            .justify_start()
            .on_key_down(on_root_key)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(
                view()
                    .absolute()
                    .inset(px(0.0))
                    .app_class("bg-black/35")
                    .on_press(move || emit_once(&tx_backdrop, PaletteOutcome::Dismissed)),
            )
            .child(card)
    }
}

/// Active `⌘K` command palette overlay owned by `KumuApp`.
pub struct ActiveCommandPalette {
    mount_id: u64,
    pub app_view: Entity<AppView>,
    _task: Task<()>,
}

impl Drop for ActiveCommandPalette {
    fn drop(&mut self) {
        let _ = MOUNTS.take(self.mount_id);
    }
}

impl ActiveCommandPalette {
    pub fn open<V: 'static>(
        items: Vec<PaletteItem>,
        theme: &KumuTheme,
        window: &mut Window,
        cx: &mut Context<V>,
        on_outcome: impl FnOnce(&mut V, PaletteOutcome, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (outcome_tx, mut outcome_rx) = unbounded::<PaletteOutcome>();
        let mount_id = MOUNTS.register(PaletteMount { items, outcome_tx });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Command Palette".to_owned(),
                transparent: true,
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(
                Root::new::<view::PaletteRoot>(move || view::PaletteRootProps {
                    mount_id: mount_id.into(),
                }),
            ));
            view
        });

        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = outcome_rx.next().await.unwrap_or(PaletteOutcome::Dismissed);
            let _ = this.update_in(cx, |host, window, cx| {
                on_outcome(host, outcome, window, cx);
            });
        });

        Self {
            mount_id,
            app_view,
            _task: task,
        }
    }

    pub fn sync_theme(&self, theme: &KumuTheme, cx: &mut gpui::App) {
        let (mapped, mode) = (nsh_theme(theme), appearance(theme.mode));
        self.app_view.update(cx, |view, cx| {
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    fn sample_items() -> Vec<PaletteItem> {
        vec![
            PaletteItem {
                section: PaletteSection::Agents,
                title: "修复测试".into(),
                subtitle: Some("kumu · 工作区 1".into()),
                keywords: "claude waiting".into(),
                icon: PaletteIcon::AgentLogo(Some("claude.png")),
                agent_state: Some(AgentState::Waiting),
                badge: None,
                shortcut: None,
                target: PaletteTarget::RevealAgent(PaneId(7)),
            },
            PaletteItem {
                section: PaletteSection::Projects,
                title: "kumu".into(),
                subtitle: Some("~/github/kumu".into()),
                keywords: "project".into(),
                icon: PaletteIcon::Svg("icons/folder.svg"),
                agent_state: None,
                badge: Some("当前".into()),
                shortcut: None,
                target: PaletteTarget::SelectProject(1),
            },
            PaletteItem {
                section: PaletteSection::Actions,
                title: "打开设置".into(),
                subtitle: None,
                keywords: "settings preferences config".into(),
                icon: PaletteIcon::Svg("icons/settings.svg"),
                agent_state: None,
                badge: None,
                shortcut: Some("⌘,"),
                target: PaletteTarget::OpenSettings,
            },
        ]
    }

    #[test]
    fn filter_items_ranks_prefix_and_supports_multi_term_queries() {
        let items = sample_items();
        assert_eq!(filter_items(&items, ""), vec![0, 1, 2]);
        assert_eq!(filter_items(&items, "kumu"), vec![1, 0]);
        assert_eq!(filter_items(&items, "claude 修复"), vec![0]);
        assert_eq!(filter_items(&items, "set"), vec![2]);
        assert!(filter_items(&items, "不存在的关键词").is_empty());
    }

    #[test]
    fn command_palette_renders_filters_and_emits_selection_headlessly() {
        let (tx, mut rx) = unbounded::<PaletteOutcome>();
        let mount_id = MOUNTS.register(PaletteMount {
            items: sample_items(),
            outcome_tx: tx,
        });
        let root =
            nsh_core::element::Root::new::<view::PaletteRoot>(move || view::PaletteRootProps {
                mount_id: mount_id.into(),
            });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start palette NSH");
        app.set_viewport(nsh_core::Size::new(
            nsh_core::Points(1200.0),
            nsh_core::Points(800.0),
        ))
        .expect("set viewport");

        app.wait_for("活动 Agent").expect("section header renders");
        app.wait_for("修复测试").expect("agent row renders");
        app.wait_for("打开设置").expect("action row renders");

        app.press("打开设置").expect("click action item");
        assert_eq!(
            rx.try_recv().ok(),
            Some(PaletteOutcome::Selected(PaletteTarget::OpenSettings))
        );
    }
}
