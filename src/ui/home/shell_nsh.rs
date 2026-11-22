//! Unified Pure Rust NSH root for Kumu's Home window (`NshHomeShell`).
//!
//! Hosts the entire workspace inside ONE single `nsh-platform-gpui::AppView`:
//! - Optional persistence error banner
//! - Outer `nsh_base::h_resizable()` (Sidebar | Center Tabs + Split Terminal Grid | Right Plugin Panel)
//! - Recursive `nsh_base::h_resizable()` / `v_resizable()` split pane tree
//! - Pure Rust NSH Pane Headers (status badge, new-agent `popup_menu`, split/close + tooltips)
//! - Native GPUI `TerminalView` leaves (`native("kumu.terminal")`, `focus_handle() -> None`)
//! - Floating Pure Rust NSH `⌘F` Find Bar over each pane
//! - Native right-hand plugin slot (`native("kumu.plugin")`)
//! - Full-window NSH foreground `toast_stack`
//!
//! Layout topology (`LayoutStructure`) is separated from per-pane chrome
//! (`HashMap<PaneId, PaneChromeSnapshot>`) so title, agent-status, active-border,
//! and `⌘F` find-bar updates never unmount or recreate live `TerminalView` nodes.

use std::collections::HashMap;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{App, AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::native_host::{NativeViewRegistry, PLUGIN_COMPONENT, TERMINAL_COMPONENT};
use super::sidebar_nsh::{SidebarAction, SidebarSnapshot};
use super::tabs_nsh::{TabBarAction, TabBarSnapshot};
use super::terminal::PaneChromeSnapshot;
use super::{SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH};
use crate::app::layout::{PaneId, SplitAxis};
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::nsh_ui::toast::{ToastCommand, ToastItem};
use crate::theme::KumuTheme;

const PLUGIN_MIN_W: f32 = 240.0;
const PLUGIN_MAX_W: f32 = 520.0;

/// Split tree structure (topology + initial group ratios only).
/// Separating this from per-pane chrome guarantees header/status/find updates
/// never remount live `native("kumu.terminal")` leaves, and topology-only
/// [`PartialEq`] ensures dragging a split divider never rebuilds the tree.
#[derive(Clone, Debug)]
pub(super) enum LayoutNodeSnapshot {
    Leaf(PaneId),
    Group {
        id: u64,
        axis: SplitAxis,
        ratios: Vec<f32>,
        children: Vec<LayoutNodeSnapshot>,
    },
}

impl PartialEq for LayoutNodeSnapshot {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Leaf(a), Self::Leaf(b)) => a == b,
            (
                Self::Group {
                    id: id_a,
                    axis: axis_a,
                    children: children_a,
                    ..
                },
                Self::Group {
                    id: id_b,
                    axis: axis_b,
                    children: children_b,
                    ..
                },
            ) => id_a == id_b && axis_a == axis_b && children_a == children_b,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum CenterModeSnapshot {
    #[default]
    EmptyProjects,
    EmptyTerminals,
    Panes(LayoutNodeSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PluginPanelSnapshot {
    pub(super) id: &'static str,
    pub(super) title: &'static str,
    pub(super) icon: &'static str,
}

#[derive(Clone, Debug, Default)]
pub(super) struct HomeShellSnapshot {
    pub(super) store_error: Option<String>,
    pub(super) sidebar_width: f32,
    pub(super) plugin_width: f32,
    pub(super) sidebar: SidebarSnapshot,
    pub(super) tab_bar: TabBarSnapshot,
    pub(super) center: CenterModeSnapshot,
    pub(super) pane_chrome: HashMap<PaneId, PaneChromeSnapshot>,
    pub(super) open_plugin: Option<PluginPanelSnapshot>,
}

#[derive(Clone, Debug)]
pub(super) enum ShellHostEvent {
    StoreError(Option<String>),
    Sidebar(SidebarSnapshot),
    TabBar(TabBarSnapshot),
    Center(CenterModeSnapshot),
    PaneChrome(HashMap<PaneId, PaneChromeSnapshot>),
    OpenPlugin(Option<PluginPanelSnapshot>),
    Toast(ToastCommand),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum HomeShellAction {
    Sidebar(SidebarAction),
    TabBar(TabBarAction),
    DismissStoreError,
    EmptyAddTerminal,
    EmptyAddProject,
    ClosePlugin,
    ResizeOuter {
        sidebar_width: f32,
        plugin_width: Option<f32>,
    },
    ResizeGroup {
        group_id: u64,
        ratios: Vec<f32>,
    },
    FocusPane(PaneId),
    SplitRight(PaneId),
    SplitDown(PaneId),
    ClosePane(PaneId),
    AgentPane {
        pane: PaneId,
        agent: String,
    },
    FindChange {
        pane: PaneId,
        needle: String,
    },
    FindStep {
        pane: PaneId,
        older: bool,
    },
    FindClose(PaneId),
    ToastClicked(String),
}

struct ShellMount {
    initial: HomeShellSnapshot,
    events_rx: UnboundedReceiver<ShellHostEvent>,
    actions_tx: UnboundedSender<HomeShellAction>,
}

static MOUNTS: MountRegistry<ShellMount> = MountRegistry::new();

mod view {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use futures::StreamExt;
    use futures::channel::mpsc::{UnboundedSender, unbounded};
    use nsh_base::{Anchor, h_resizable, resizable_panel, theme as active_theme, v_resizable};
    use nsh_core::element::desktop::{MenuItem, popup_menu};
    use nsh_core::element::{AccessibilityRole, native};
    use nsh_core::prelude::*;

    use super::{
        CenterModeSnapshot, HomeShellAction, HomeShellSnapshot, LayoutNodeSnapshot, MOUNTS,
        PLUGIN_COMPONENT, PLUGIN_MAX_W, PLUGIN_MIN_W, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
        ShellHostEvent, TERMINAL_COMPONENT,
    };
    use crate::agent::{AGENTS, AgentState, installed};
    use crate::app::layout::{PaneId, SplitAxis};
    use crate::nsh_ui::assets::{asset_src, icon_svg};
    use crate::nsh_ui::components::{
        ButtonVariant, body_text, kumu_button, kumu_input, kumu_tooltip,
    };
    use crate::nsh_ui::theme::{
        TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_xs_size,
    };
    use crate::nsh_ui::toast::{ToastCommand, ToastHostEvent, render_toast_stack};
    use crate::ui::home::sidebar_nsh::view::render_sidebar;
    use crate::ui::home::tabs_nsh::view::render_tab_bar;
    use crate::ui::home::terminal::PaneChromeSnapshot;

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct HomeShellRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<HomeShellAction>>, action: HomeShellAction) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(action);
        }
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

    fn render_agent_badge(state: AgentState) -> AnyElement {
        let color = move || match state {
            AgentState::Running => link_color(),
            AgentState::Waiting => warning_color(),
            AgentState::Completed => success_color(),
            AgentState::Failed => active_theme().destructive,
        };
        let tint = if state == AgentState::Running {
            0.14
        } else {
            0.16
        };
        let glyph: AnyElement = match state {
            AgentState::Running => icon_svg("icons/loader-circle.svg", 12.0)
                .app_class("animate-spin")
                .text_color(color)
                .into_element(),
            AgentState::Waiting => view()
                .w(px(7.0))
                .h(px(7.0))
                .rounded(px(3.5))
                .bg(color)
                .into_element(),
            AgentState::Completed => icon_svg("icons/check.svg", 12.0)
                .text_color(color)
                .into_element(),
            AgentState::Failed => icon_svg("icons/triangle-alert.svg", 12.0)
                .text_color(color)
                .into_element(),
        };
        view()
            .flex_row()
            .flex_shrink_0()
            .h(px(20.0))
            .gap_1()
            .px_1p5()
            .items_center()
            .rounded(move || active_theme().radius)
            .bg(move || color().opacity(tint))
            .child(glyph)
            .child(
                text(state.label())
                    .text_size(text_xs_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color),
            )
            .into_element()
    }

    fn header_icon_button(
        icon: &'static str,
        tooltip_label: &'static str,
        on_press: impl Fn() + 'static,
    ) -> View {
        view()
            .role(AccessibilityRole::Button)
            .label(tooltip_label)
            .tooltip(kumu_tooltip(tooltip_label))
            .flex_shrink_0()
            .p_0p5()
            .rounded(move || active_theme().radius)
            .app_class("cursor-pointer")
            .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
            .on_press(on_press)
            .child(icon_svg(icon, 14.0).text_color(move || active_theme().muted_foreground))
    }

    type PaneSignalMap = Rc<RefCell<HashMap<PaneId, Signal<Option<PaneChromeSnapshot>>>>>;

    fn pane_signal(signals: &PaneSignalMap, pane_id: PaneId) -> Signal<Option<PaneChromeSnapshot>> {
        *signals
            .borrow_mut()
            .entry(pane_id)
            .or_insert_with(|| signal(None))
    }

    fn render_pane_leaf(
        pane_id: PaneId,
        pane_sig: Signal<Option<PaneChromeSnapshot>>,
        tx: Option<UnboundedSender<HomeShellAction>>,
    ) -> AnyElement {
        let tx_hdr = tx.clone();
        let tx_find = tx.clone();

        let header = dynamic(move || {
            let Some(snap) = pane_sig.get() else {
                return view().h(px(34.0)).w_full().into_element();
            };
            let active = snap.active;
            let can_close = snap.can_close;
            let tx_focus = tx_hdr.clone();
            let tx_agent = tx_hdr.clone();
            let tx_sr = tx_hdr.clone();
            let tx_sd = tx_hdr.clone();
            let tx_close = tx_hdr.clone();

            let mut row = view()
                .flex_row()
                .flex_shrink_0()
                .w_full()
                .h(px(34.0))
                .px_3()
                .gap_2()
                .items_center()
                .bg(move || {
                    if active {
                        active_theme().secondary.opacity(0.45)
                    } else {
                        AdaptiveColor::TRANSPARENT
                    }
                })
                .on_press(move || send(&tx_focus, HomeShellAction::FocusPane(pane_id)))
                .child(
                    icon_svg("icons/square-terminal.svg", 14.0)
                        .text_color(move || active_theme().muted_foreground),
                )
                .child(
                    text(snap.name)
                        .flex_shrink_0()
                        .max_w(px(180.0))
                        .min_w_0()
                        .app_class("truncate")
                        .text_size(text_xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || active_theme().foreground),
                )
                .child(
                    text(snap.cwd)
                        .flex_1()
                        .min_w_0()
                        .app_class("truncate")
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().muted_foreground),
                );

            if let Some(note) = snap.note {
                row = row.child(
                    text(note)
                        .flex_shrink_0()
                        .text_size(text_xs_size)
                        .text_color(warning_color),
                );
            }
            if let Some(status) = snap.agent_status {
                row = row.child(render_agent_badge(status));
            }

            row = row
                .child(header_icon_button(
                    "icons/bot.svg",
                    "新建 Agent 窗格",
                    move || {
                        let available: Vec<_> = AGENTS.iter().filter(|a| installed(a.id)).collect();
                        let items = if available.is_empty() {
                            vec![MenuItem::new("未安装 Agent").disabled(true)]
                        } else {
                            available
                                .into_iter()
                                .map(|agent| {
                                    let tx = tx_agent.clone();
                                    let id = agent.id.to_string();
                                    let icon = asset_src(agent.logo.unwrap_or("icons/bot.svg"));
                                    MenuItem::new(agent.name)
                                        .icon(icon)
                                        .on_select(move || {
                                        send(
                                            &tx,
                                            HomeShellAction::AgentPane {
                                                pane: pane_id,
                                                agent: id.clone(),
                                            },
                                        );
                                    })
                                })
                                .collect()
                        };
                        drop(popup_menu(items));
                    },
                ))
                .child(header_icon_button(
                    "split-right.svg",
                    "向右分屏 ⌘D",
                    move || send(&tx_sr, HomeShellAction::SplitRight(pane_id)),
                ))
                .child(header_icon_button(
                    "split-down.svg",
                    "向下分屏 ⌘⇧D",
                    move || send(&tx_sd, HomeShellAction::SplitDown(pane_id)),
                ));

            if can_close {
                row = row.child(header_icon_button(
                    "icons/close.svg",
                    "关闭窗格",
                    move || send(&tx_close, HomeShellAction::ClosePane(pane_id)),
                ));
            }

            row.into_element()
        });

        let find_open = move || pane_sig.with(|p| p.as_ref().is_some_and(|c| c.find.is_some()));
        let find_bar = when(find_open, move || {
            let initial_find = pane_sig.get_untracked().and_then(|c| c.find);
            let initial_needle = initial_find
                .as_ref()
                .map(|f| f.needle.clone())
                .unwrap_or_default();
            let initial_nonce = initial_find.as_ref().map_or(0, |f| f.focus_nonce);

            let draft = signal(initial_needle);
            let input_ref = element_ref();
            let last_nonce = stored(initial_nonce);

            // Re-focus (and select-all) whenever ⌘F is pressed again while open.
            effect(move || {
                let nonce = pane_sig.with(|p| {
                    p.as_ref()
                        .and_then(|c| c.find.as_ref())
                        .map_or(0, |f| f.focus_nonce)
                });
                let changed = last_nonce.with(|prev| {
                    if nonce > *prev {
                        *prev = nonce;
                        true
                    } else {
                        false
                    }
                });
                if changed {
                    input_ref.focus();
                }
            });

            let count_label = move || {
                pane_sig.with(|p| {
                    p.as_ref()
                        .and_then(|c| c.find.as_ref())
                        .map(|f| f.count_label.clone())
                        .unwrap_or_default()
                })
            };
            let has_matches = move || {
                pane_sig.with(|p| {
                    p.as_ref()
                        .and_then(|c| c.find.as_ref())
                        .is_some_and(|f| f.has_matches)
                })
            };

            let tx_change = tx_find.clone();
            let tx_key = tx_find.clone();
            let tx_older = tx_find.clone();
            let tx_newer = tx_find.clone();
            let tx_close = tx_find.clone();

            view()
                .absolute()
                .top_2()
                .right_3()
                .w(px(340.0))
                .app_class("max-w-[92%]")
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(move || active_theme().radius)
                .border_1()
                .border_color(move || active_theme().border)
                .bg(move || active_theme().secondary)
                .shadow_md()
                .on_key_down(move |ev: &nsh_core::KeyEvent| {
                    if ev.key == "Escape" {
                        send(&tx_key, HomeShellAction::FindClose(pane_id));
                    } else if ev.key == "Enter" {
                        send(
                            &tx_key,
                            HomeShellAction::FindStep {
                                pane: pane_id,
                                older: !ev.modifiers.shift,
                            },
                        );
                    }
                })
                .child(
                    kumu_input("在当前终端中查找")
                        .element_ref(&input_ref)
                        .auto_focus(true)
                        .select_text_on_focus(true)
                        .flex_1()
                        .min_w_0()
                        .py_0p5()
                        .px_1p5()
                        .value(move || draft.get())
                        .on_change(move |v: &str| {
                            draft.set(v.to_owned());
                            send(
                                &tx_change,
                                HomeShellAction::FindChange {
                                    pane: pane_id,
                                    needle: v.to_owned(),
                                },
                            );
                        }),
                )
                .child(
                    text(count_label)
                        .style_when(move || count_label().is_empty(), |s| s.hidden())
                        .flex_shrink_0()
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().muted_foreground),
                )
                .child(
                    header_icon_button(
                        "icons/chevron-up.svg",
                        "查找更早的内容（Enter）",
                        move || {
                            send(
                                &tx_older,
                                HomeShellAction::FindStep {
                                    pane: pane_id,
                                    older: true,
                                },
                            );
                            input_ref.focus();
                        },
                    )
                    .opacity(move || if has_matches() { 1.0_f32 } else { 0.4_f32 }),
                )
                .child(
                    header_icon_button(
                        "icons/chevron-down.svg",
                        "查找更新的内容（⇧Enter）",
                        move || {
                            send(
                                &tx_newer,
                                HomeShellAction::FindStep {
                                    pane: pane_id,
                                    older: false,
                                },
                            );
                            input_ref.focus();
                        },
                    )
                    .opacity(move || if has_matches() { 1.0_f32 } else { 0.4_f32 }),
                )
                .child(header_icon_button(
                    "icons/x.svg",
                    "关闭查找",
                    move || {
                        send(&tx_close, HomeShellAction::FindClose(pane_id));
                    },
                ))
        });

        let is_active = move || pane_sig.with(|p| p.as_ref().is_some_and(|c| c.active));

        view()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .border_1()
            .border_color(move || {
                if is_active() {
                    link_color()
                } else {
                    AdaptiveColor::TRANSPARENT
                }
            })
            .bg(move || active_theme().background)
            .child(header)
            .child(
                view()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(
                        native(TERMINAL_COMPONENT)
                            .size_full()
                            .prop("pane", serde_json::json!(pane_id.0)),
                    )
                    .child(find_bar),
            )
            .into_element()
    }

    fn render_pane_tree(
        node: &LayoutNodeSnapshot,
        pane_signals: &PaneSignalMap,
        tx: Option<UnboundedSender<HomeShellAction>>,
    ) -> AnyElement {
        match node {
            LayoutNodeSnapshot::Leaf(pane_id) => {
                render_pane_leaf(*pane_id, pane_signal(pane_signals, *pane_id), tx)
            }
            LayoutNodeSnapshot::Group {
                id,
                axis,
                ratios,
                children,
            } => {
                let group_id = *id;
                let tx_resize = tx.clone();
                let mut group = match axis {
                    SplitAxis::Horizontal => h_resizable().handle_class(
                        "w-px bg-border group-hover:bg-primary group-active:bg-primary",
                    ),
                    SplitAxis::Vertical => v_resizable().handle_class(
                        "h-px bg-border group-hover:bg-primary group-active:bg-primary",
                    ),
                };
                group = group.on_resize(move |sizes: &Vec<Points>| {
                    let total: f32 = sizes.iter().map(|s| s.0).sum();
                    if total > 0.0 && total.is_finite() {
                        let next: Vec<f32> = sizes.iter().map(|s| s.0 / total).collect();
                        if next.iter().all(|r| r.is_finite() && *r > 0.0) {
                            send(
                                &tx_resize,
                                HomeShellAction::ResizeGroup {
                                    group_id,
                                    ratios: next,
                                },
                            );
                        }
                    }
                });

                let count = children.len().max(1);
                for (index, child) in children.iter().enumerate() {
                    let child_el = render_pane_tree(child, pane_signals, tx.clone());
                    let mut panel = resizable_panel().min_size(px(120.0));
                    if ratios.len() == count {
                        panel = panel.size(Some(px(ratios[index] * 1000.0)));
                    }
                    // Reserve 1px after leading divider so a focused pane's 1px
                    // border stays visible on all four sides.
                    let wrapped = if index > 0 {
                        match axis {
                            SplitAxis::Horizontal => view()
                                .size_full()
                                .pl(px(1.0))
                                .child(child_el)
                                .into_element(),
                            SplitAxis::Vertical => view()
                                .size_full()
                                .pt(px(1.0))
                                .child(child_el)
                                .into_element(),
                        }
                    } else {
                        child_el
                    };
                    group = group.child(panel.child(wrapped));
                }
                group.into_element()
            }
        }
    }

    #[component]
    pub(super) fn HomeShellRoot(props: HomeShellRootProps) -> impl IntoElement {
        let mount = MOUNTS.take(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (HomeShellSnapshot::default(), None, None),
        };

        let store_error = signal(initial.store_error);
        let sidebar_width = signal(
            initial
                .sidebar_width
                .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH),
        );
        let plugin_width = signal(initial.plugin_width.clamp(PLUGIN_MIN_W, PLUGIN_MAX_W));
        let sidebar_sig = signal(initial.sidebar);
        let tab_bar_sig = signal(initial.tab_bar);
        let center_sig = signal(initial.center);
        let pane_signals: PaneSignalMap = Rc::new(RefCell::new(
            initial
                .pane_chrome
                .into_iter()
                .map(|(id, snap)| (id, signal(Some(snap))))
                .collect(),
        ));
        let open_plugin = signal(initial.open_plugin);

        let (sidebar_tx, mut sidebar_rx) = unbounded();
        let (tabs_tx, mut tabs_rx) = unbounded();
        let (toast_cmd_tx, toast_cmd_rx) = unbounded::<ToastCommand>();
        let (toast_ev_tx, mut toast_ev_rx) = unbounded::<ToastHostEvent>();

        // Forward sub-view actions into unified HomeShellAction channel.
        let tx_sb = actions_tx.clone();
        spawn(async move {
            while let Some(a) = sidebar_rx.next().await {
                send(&tx_sb, HomeShellAction::Sidebar(a));
            }
        });
        let tx_tb = actions_tx.clone();
        spawn(async move {
            while let Some(a) = tabs_rx.next().await {
                send(&tx_tb, HomeShellAction::TabBar(a));
            }
        });
        let tx_toast = actions_tx.clone();
        spawn(async move {
            while let Some(ToastHostEvent::Clicked(id)) = toast_ev_rx.next().await {
                send(&tx_toast, HomeShellAction::ToastClicked(id));
            }
        });

        let pane_signals_ev = pane_signals.clone();
        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(ev) = rx.next().await {
                    match ev {
                        ShellHostEvent::StoreError(err) => store_error.set(err),
                        ShellHostEvent::Sidebar(s) => sidebar_sig.set(s),
                        ShellHostEvent::TabBar(t) => tab_bar_sig.set(t),
                        ShellHostEvent::Center(c) => center_sig.set(c),
                        ShellHostEvent::PaneChrome(mut next_map) => {
                            let mut map = pane_signals_ev.borrow_mut();
                            for (pane_id, sig) in map.iter() {
                                if let Some(next) = next_map.remove(pane_id) {
                                    if sig.get_untracked().as_ref() != Some(&next) {
                                        sig.set(Some(next));
                                    }
                                } else if sig.get_untracked().is_some() {
                                    sig.set(None);
                                }
                            }
                            for (pane_id, next) in next_map {
                                map.insert(pane_id, signal(Some(next)));
                            }
                        }
                        ShellHostEvent::OpenPlugin(p) => open_plugin.set(p),
                        ShellHostEvent::Toast(cmd) => {
                            let _ = toast_cmd_tx.unbounded_send(cmd);
                        }
                    }
                }
            });
        }

        let tx_err = actions_tx.clone();
        let error_banner = dynamic(move || {
            let Some(msg) = store_error.get() else {
                return view().into_element();
            };
            let tx_x = tx_err.clone();
            view()
                .flex_row()
                .flex_shrink_0()
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .bg(move || warning_color().opacity(0.12))
                .child(
                    text(msg)
                        .flex_1()
                        .min_w_0()
                        .text_size(text_xs_size)
                        .text_color(warning_color),
                )
                .child(
                    view()
                        .role(AccessibilityRole::Button)
                        .label("关闭错误提示")
                        .flex_shrink_0()
                        .p_0p5()
                        .rounded(move || active_theme().radius)
                        .app_class("cursor-pointer")
                        .hover(|s| s.bg(move || warning_color().opacity(0.25)))
                        .on_press(move || send(&tx_x, HomeShellAction::DismissStoreError))
                        .child(icon_svg("icons/close.svg", 12.0).text_color(warning_color)),
                )
                .into_element()
        });

        let sidebar_el = render_sidebar(sidebar_sig, Some(sidebar_tx));
        let tab_bar_el = render_tab_bar(tab_bar_sig, Some(tabs_tx));

        let tx_center = actions_tx.clone();
        let pane_signals_center = pane_signals.clone();
        let center_body = dynamic(move || {
            let tx_c = tx_center.clone();
            match center_sig.get() {
                CenterModeSnapshot::Panes(root_node) => {
                    render_pane_tree(&root_node, &pane_signals_center, tx_c)
                }
                CenterModeSnapshot::EmptyTerminals => view()
                    .flex_col()
                    .flex_1()
                    .w_full()
                    .h_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        body_text("还没有终端").text_color(move || active_theme().muted_foreground),
                    )
                    .child(
                        kumu_button("新建终端", ButtonVariant::Ghost)
                            .on_press(move || send(&tx_c, HomeShellAction::EmptyAddTerminal)),
                    )
                    .into_element(),
                CenterModeSnapshot::EmptyProjects => view()
                    .flex_col()
                    .flex_1()
                    .w_full()
                    .h_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        body_text("还没有项目").text_color(move || active_theme().muted_foreground),
                    )
                    .child(
                        kumu_button("打开项目", ButtonVariant::Ghost)
                            .on_press(move || send(&tx_c, HomeShellAction::EmptyAddProject)),
                    )
                    .into_element(),
            }
        });

        let has_panes = move || matches!(center_sig.get(), CenterModeSnapshot::Panes(_));
        let center_column = view()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(
                view()
                    .style_when(move || !has_panes(), |s| s.hidden())
                    .flex_shrink_0()
                    .w_full()
                    .h(px(36.0))
                    .child(tab_bar_el),
            )
            .child(view().flex_1().min_h_0().min_w_0().child(center_body));

        let tx_plug_close = actions_tx.clone();
        let plugin_panel_content = dynamic(move || {
            let Some(plugin) = open_plugin.get() else {
                return view().into_element();
            };
            let tx_x = tx_plug_close.clone();
            view()
                .flex_col()
                .size_full()
                .min_h_0()
                .min_w_0()
                .bg(move || active_theme().sidebar)
                .child(
                    view()
                        .flex_row()
                        .flex_shrink_0()
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .border_b_1()
                        .border_b_color(move || active_theme().sidebar_border)
                        .child(
                            icon_svg(plugin.icon, 14.0)
                                .text_color(move || active_theme().muted_foreground),
                        )
                        .child(
                            text(plugin.title)
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || active_theme().sidebar_foreground),
                        )
                        .child(view().flex_1())
                        .child(header_icon_button(
                            "icons/close.svg",
                            "收起面板",
                            move || {
                                send(&tx_x, HomeShellAction::ClosePlugin);
                            },
                        )),
                )
                .child(
                    native(PLUGIN_COMPONENT)
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .prop("id", serde_json::json!(plugin.id)),
                )
                .into_element()
        });

        let tx_outer = actions_tx.clone();
        let main_split = h_resizable()
            .handle_class("w-px bg-border group-hover:bg-primary group-active:bg-primary")
            .on_resize(move |sizes: &Vec<Points>| {
                let Some(sb) = sizes.first().map(|s| s.0) else {
                    return;
                };
                let pw = sizes.get(2).map(|s| s.0);
                send(
                    &tx_outer,
                    HomeShellAction::ResizeOuter {
                        sidebar_width: sb,
                        plugin_width: pw,
                    },
                );
            })
            .child(
                resizable_panel()
                    .size(move || Some(px(sidebar_width.get())))
                    .size_range(px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH))
                    .fixed(true)
                    .child(sidebar_el),
            )
            .child(resizable_panel().min_size(px(280.0)).child(center_column))
            .child(
                resizable_panel()
                    .visible(move || open_plugin.get().is_some())
                    .size(move || Some(px(plugin_width.get())))
                    .size_range(px(PLUGIN_MIN_W)..px(PLUGIN_MAX_W))
                    .fixed(true)
                    .child(plugin_panel_content),
            );

        view()
            .relative()
            .flex_col()
            .size_full()
            .bg(move || active_theme().background)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(error_banner)
            .child(view().flex_1().min_h_0().min_w_0().child(main_split))
            .child(render_toast_stack(
                Some(toast_cmd_rx),
                Anchor::TopRight,
                Some(toast_ev_tx),
            ))
    }
}

/// Unified Pure Rust NSH home shell handle owned by `KumuApp`.
pub(super) struct NshHomeShell {
    mount_id: u64,
    pub(super) app_view: Entity<AppView>,
    events_tx: UnboundedSender<ShellHostEvent>,
    last_error: Option<String>,
    last_sidebar: SidebarSnapshot,
    last_tab_bar: TabBarSnapshot,
    last_center: CenterModeSnapshot,
    last_pane_chrome: HashMap<PaneId, PaneChromeSnapshot>,
    last_plugin: Option<PluginPanelSnapshot>,
    _actions_task: Task<()>,
}

impl Drop for NshHomeShell {
    fn drop(&mut self) {
        let _ = MOUNTS.take(self.mount_id);
    }
}

impl NshHomeShell {
    pub(super) fn new<V: 'static>(
        initial: HomeShellSnapshot,
        registry: &NativeViewRegistry,
        theme: &KumuTheme,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_action: impl FnMut(&mut V, HomeShellAction, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (events_tx, events_rx) = unbounded::<ShellHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<HomeShellAction>();
        let last_error = initial.store_error.clone();
        let last_sidebar = initial.sidebar.clone();
        let last_tab_bar = initial.tab_bar.clone();
        let last_center = initial.center.clone();
        let last_pane_chrome = initial.pane_chrome.clone();
        let last_plugin = initial.open_plugin.clone();

        let mount_id = MOUNTS.register(ShellMount {
            initial,
            events_rx,
            actions_tx,
        });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let registry = registry.clone();
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            registry.install_on(&view);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(
                Root::new::<view::HomeShellRoot>(move || view::HomeShellRootProps {
                    mount_id: mount_id.into(),
                }),
            ));
            view
        });

        let actions_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let res = this.update_in(cx, |host, window, cx| {
                    on_action(host, action, window, cx);
                });
                if res.is_err() {
                    break;
                }
            }
        });

        Self {
            mount_id,
            app_view,
            events_tx,
            last_error,
            last_sidebar,
            last_tab_bar,
            last_center,
            last_pane_chrome,
            last_plugin,
            _actions_task: actions_task,
        }
    }

    pub(super) fn sync(&mut self, next: HomeShellSnapshot) {
        if self.last_error != next.store_error {
            self.last_error = next.store_error.clone();
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::StoreError(next.store_error));
        }
        if self.last_sidebar != next.sidebar {
            self.last_sidebar = next.sidebar.clone();
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::Sidebar(next.sidebar));
        }
        if self.last_tab_bar != next.tab_bar {
            self.last_tab_bar = next.tab_bar.clone();
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::TabBar(next.tab_bar));
        }
        if self.last_pane_chrome != next.pane_chrome {
            self.last_pane_chrome = next.pane_chrome.clone();
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::PaneChrome(next.pane_chrome));
        }
        if self.last_center != next.center {
            self.last_center = next.center.clone();
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::Center(next.center));
        }
        if self.last_plugin != next.open_plugin {
            self.last_plugin = next.open_plugin.clone();
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::OpenPlugin(next.open_plugin));
        }
    }

    pub(super) fn push_toast(&self, item: ToastItem) {
        let _ = self
            .events_tx
            .unbounded_send(ShellHostEvent::Toast(ToastCommand::Push(item)));
    }

    pub(super) fn dismiss_toast(&self, id: String) {
        let _ = self
            .events_tx
            .unbounded_send(ShellHostEvent::Toast(ToastCommand::Dismiss(id)));
    }

    pub(super) fn sync_theme(&self, theme: &KumuTheme, cx: &mut App) {
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
    use crate::ui::home::terminal::FindBarSnapshot;
    use nsh_engine::EngineConfig;

    #[test]
    fn home_shell_renders_pane_headers_find_bar_and_empty_states() {
        let (events_tx, events_rx) = unbounded::<ShellHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<HomeShellAction>();
        let mut chrome = HashMap::new();
        chrome.insert(
            PaneId(1),
            PaneChromeSnapshot {
                id: PaneId(1),
                name: "zsh".into(),
                cwd: "~/kumu".into(),
                note: None,
                agent_status: None,
                active: true,
                can_close: true,
                find: Some(FindBarSnapshot {
                    needle: "hello".into(),
                    count_label: "1/3".into(),
                    has_matches: true,
                    focus_nonce: 1,
                }),
            },
        );
        let initial = HomeShellSnapshot {
            sidebar_width: 256.0,
            plugin_width: 300.0,
            center: CenterModeSnapshot::Panes(LayoutNodeSnapshot::Leaf(PaneId(1))),
            pane_chrome: chrome,
            ..Default::default()
        };
        let mount_id = MOUNTS.register(ShellMount {
            initial,
            events_rx,
            actions_tx,
        });
        let root =
            nsh_core::element::Root::new::<view::HomeShellRoot>(move || view::HomeShellRootProps {
                mount_id: mount_id.into(),
            });
        let mut app = nsh_testing::Headless::start(EngineConfig::new(root))
            .expect("start unified home shell");
        app.set_viewport(nsh_core::Size::new(
            nsh_core::Points(1200.0),
            nsh_core::Points(800.0),
        ))
        .expect("set desktop viewport");

        app.wait_for("zsh").expect("pane header name renders");
        app.wait_for("1/3").expect("find bar match count renders");

        // Updating match count updates badge reactively without tearing down the find bar.
        let mut updated_chrome = HashMap::new();
        updated_chrome.insert(
            PaneId(1),
            PaneChromeSnapshot {
                id: PaneId(1),
                name: "zsh".into(),
                cwd: "~/kumu".into(),
                note: None,
                agent_status: None,
                active: true,
                can_close: true,
                find: Some(FindBarSnapshot {
                    needle: "hello".into(),
                    count_label: "2/3".into(),
                    has_matches: true,
                    focus_nonce: 2,
                }),
            },
        );
        events_tx
            .unbounded_send(ShellHostEvent::PaneChrome(updated_chrome))
            .expect("update match count");
        app.wait_for("2/3").expect("updated match count renders");

        app.press("label:关闭查找").expect("press close find");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(HomeShellAction::FindClose(PaneId(1)))
        );

        events_tx
            .unbounded_send(ShellHostEvent::Center(CenterModeSnapshot::EmptyProjects))
            .expect("switch to empty projects");
        app.wait_for("还没有项目").expect("empty projects renders");
    }

    #[test]
    fn layout_topology_equality_ignores_dragged_split_ratios() {
        let before = LayoutNodeSnapshot::Group {
            id: 1,
            axis: SplitAxis::Horizontal,
            ratios: vec![0.5, 0.5],
            children: vec![
                LayoutNodeSnapshot::Leaf(PaneId(1)),
                LayoutNodeSnapshot::Leaf(PaneId(2)),
            ],
        };
        let after_drag = LayoutNodeSnapshot::Group {
            id: 1,
            axis: SplitAxis::Horizontal,
            ratios: vec![0.3, 0.7],
            children: vec![
                LayoutNodeSnapshot::Leaf(PaneId(1)),
                LayoutNodeSnapshot::Leaf(PaneId(2)),
            ],
        };
        let after_split = LayoutNodeSnapshot::Group {
            id: 1,
            axis: SplitAxis::Horizontal,
            ratios: vec![0.3, 0.35, 0.35],
            children: vec![
                LayoutNodeSnapshot::Leaf(PaneId(1)),
                LayoutNodeSnapshot::Leaf(PaneId(2)),
                LayoutNodeSnapshot::Leaf(PaneId(3)),
            ],
        };

        assert_eq!(before, after_drag);
        assert_ne!(before, after_split);
    }
}
