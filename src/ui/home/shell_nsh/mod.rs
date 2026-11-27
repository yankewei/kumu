//! Unified Pure Rust NSH root for Kumu's Home window (`NshHomeShell`).
//!
//! Hosts the entire workspace inside ONE single `nsh-platform-gpui::AppView`:
//! - Optional persistence error banner
//! - Outer `nsh_base::h_resizable()` (Sidebar | Center Tabs + Split Terminal Grid | Right Plugin Panel)
//! - Recursive `nsh_base::h_resizable()` / `v_resizable()` split pane tree
//! - Pure Rust NSH Pane Headers (status badge, new-agent picker trigger, split/close + tooltips)
//! - Native GPUI `TerminalView` leaves (`native("kumu.terminal")`, `focus_handle() -> None`)
//! - Floating Pure Rust NSH `⌘F` Find Bar over each pane
//! - Native right-hand plugin slot (`native("kumu.plugin")`)
//!
//! Layout topology (`LayoutStructure`) is separated from per-pane chrome
//! (`HashMap<PaneId, PaneChromeSnapshot>`) so title, agent-status, active-border,
//! and `⌘F` find-bar updates never unmount or recreate live `TerminalView` nodes.

mod pane_tree;

use std::collections::HashMap;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{App, AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::native_host::{NativeViewRegistry, PLUGIN_COMPONENT, TERMINAL_COMPONENT};
use super::plugins_host::{PLUGIN_MAX_WIDTH, PLUGIN_MIN_WIDTH};
use super::sidebar_nsh::{SidebarAction, SidebarSnapshot};
use super::tabs_nsh::{TabBarAction, TabBarSnapshot};
use super::terminal::PaneChromeSnapshot;
use super::todo_board::{TodoBoardAction, TodoBoardSnapshot};
use super::{SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH};
use crate::app::layout::{PaneId, SplitAxis};
use crate::nsh_ui::mount::MountRegistry;
use crate::nsh_ui::theme::{appearance, nsh_theme};
use crate::theme::KumuTheme;

/// Soft-settle deceleration curve (`cubic-bezier(0.22, 1, 0.36, 1)`) for left
/// sidebar and right plugin drawers: entering 240ms, leaving slightly faster 200ms.
const PANEL_EASE: [f32; 4] = [0.22, 1.0, 0.36, 1.0];
const PANEL_OPEN_MS: f32 = 240.0;
const PANEL_CLOSE_MS: f32 = 200.0;
/// One-frame settle cushion after the layout animation finishes before
/// flushing a deferred terminal PTY resize or unmounting a closed plugin.
const PANEL_SETTLE_PAD_MS: u64 = 20;

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
    TodoBoard(TodoBoardSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TerminalContextAction {
    Copy,
    Paste,
    SplitRight,
    SplitDown,
    OpenTodo(i64),
    CompleteTodo(i64),
    DetachTodo(i64),
    LinkTodo(i64),
    CreateAndLinkTodo,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TerminalContextMenuRequest {
    pub(super) pane: PaneId,
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) linked_todos: Vec<(i64, String)>,
    pub(super) candidate_todos: Vec<(i64, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PluginPanelSnapshot {
    pub(super) id: &'static str,
    pub(super) title: &'static str,
    pub(super) icon: &'static str,
}

#[derive(Clone, Debug)]
pub(super) struct HomeShellSnapshot {
    pub(super) store_error: Option<String>,
    pub(super) sidebar_visible: bool,
    pub(super) sidebar_width: f32,
    pub(super) plugin_width: f32,
    pub(super) sidebar: SidebarSnapshot,
    pub(super) tab_bar: TabBarSnapshot,
    pub(super) center: CenterModeSnapshot,
    pub(super) pane_chrome: HashMap<PaneId, PaneChromeSnapshot>,
    pub(super) open_plugin: Option<PluginPanelSnapshot>,
}

impl Default for HomeShellSnapshot {
    fn default() -> Self {
        Self {
            store_error: None,
            sidebar_visible: true,
            sidebar_width: 0.0,
            plugin_width: 0.0,
            sidebar: SidebarSnapshot::default(),
            tab_bar: TabBarSnapshot::default(),
            center: CenterModeSnapshot::default(),
            pane_chrome: HashMap::new(),
            open_plugin: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum ShellHostEvent {
    StoreError(Option<String>),
    SidebarVisible(bool),
    Sidebar(SidebarSnapshot),
    TabBar(TabBarSnapshot),
    Center(CenterModeSnapshot),
    PaneChrome(HashMap<PaneId, PaneChromeSnapshot>),
    OpenPlugin(Option<PluginPanelSnapshot>),
    OpenTerminalContextMenu(TerminalContextMenuRequest),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum HomeShellAction {
    Sidebar(SidebarAction),
    TabBar(TabBarAction),
    TodoBoard(TodoBoardAction),
    DismissStoreError,
    EmptyAddTerminal,
    EmptyOpenTodoBoard,
    EmptyAddProject,
    ClosePlugin,
    /// Left or right drawer animation finished (or skipped under reduced motion):
    /// flush the deferred single terminal PTY resize.
    PanelAnimationSettled,
    /// Right plugin drawer finished sliding shut: safe to release plugin view
    /// entities and stop background refresh loops.
    PluginCloseFinished,
    ResizeSidebar(f32),
    ResizePlugin(f32),
    ResizeGroup {
        group_id: u64,
        ratios: Vec<f32>,
    },
    FocusPane(PaneId),
    ToggleZoomPane(PaneId),
    SplitRight(PaneId),
    SplitDown(PaneId),
    ClosePane(PaneId),
    /// Open the anchored agent picker below a pane header's new-agent button.
    OpenAgentPicker {
        pane: PaneId,
        anchor: crate::nsh_ui::agent_picker::AgentPickerAnchor,
    },
    OpenTodoFromPane {
        pane: PaneId,
        todo_id: i64,
    },
    TerminalContextMenu {
        pane: PaneId,
        action: TerminalContextAction,
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
}

struct ShellMount {
    initial: HomeShellSnapshot,
    events_rx: UnboundedReceiver<ShellHostEvent>,
    actions_tx: UnboundedSender<HomeShellAction>,
}

static MOUNTS: MountRegistry<ShellMount> = MountRegistry::new();

fn send(tx: &Option<UnboundedSender<HomeShellAction>>, action: HomeShellAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

mod view {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use futures::StreamExt;
    use futures::channel::mpsc::{UnboundedSender, unbounded};
    use nsh_base::{HandleEdge, resize_handle, theme as active_theme};
    use nsh_core::element::desktop::{MenuItem, popup_menu_at};
    use nsh_core::element::{LayoutCurve, LayoutTransition, native};
    use nsh_core::geometry::Axis;
    use nsh_core::prelude::*;

    use super::pane_tree::{PaneSignalMap, header_icon_button, render_pane_tree};
    use super::{
        CenterModeSnapshot, HomeShellAction, HomeShellSnapshot, MOUNTS, PANEL_CLOSE_MS, PANEL_EASE,
        PANEL_OPEN_MS, PANEL_SETTLE_PAD_MS, PLUGIN_COMPONENT, PLUGIN_MAX_WIDTH, PLUGIN_MIN_WIDTH,
        SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, ShellHostEvent, TerminalContextAction,
        TerminalContextMenuRequest, send,
    };
    use crate::nsh_ui::assets::icon_svg;
    use crate::nsh_ui::components::{
        ButtonVariant, ControlTone, body_text, kumu_button, kumu_icon_button,
    };
    use crate::nsh_ui::theme::{text_xs_size, warning_color};
    use crate::ui::home::sidebar_nsh::view::render_sidebar;
    use crate::ui::home::tabs_nsh::view::render_tab_bar;
    use crate::ui::home::todo_board::board::render_todo_board;

    fn open_terminal_popup_menu(
        req: TerminalContextMenuRequest,
        tx: Option<UnboundedSender<HomeShellAction>>,
    ) {
        let pane = req.pane;
        let emit = move |tx: &Option<UnboundedSender<HomeShellAction>>,
                         action: TerminalContextAction| {
            send(tx, HomeShellAction::TerminalContextMenu { pane, action });
        };

        let mut items = Vec::new();
        {
            let tx_c = tx.clone();
            items.push(MenuItem::new("复制").on_select(move || {
                emit(&tx_c, TerminalContextAction::Copy);
            }));
        }
        {
            let tx_p = tx.clone();
            items.push(MenuItem::new("粘贴").on_select(move || {
                emit(&tx_p, TerminalContextAction::Paste);
            }));
        }
        items.push(MenuItem::separator());
        {
            let tx_sr = tx.clone();
            items.push(MenuItem::new("向右分屏").on_select(move || {
                emit(&tx_sr, TerminalContextAction::SplitRight);
            }));
        }
        {
            let tx_sd = tx.clone();
            items.push(MenuItem::new("向下分屏").on_select(move || {
                emit(&tx_sd, TerminalContextAction::SplitDown);
            }));
        }

        if !req.linked_todos.is_empty() {
            items.push(MenuItem::separator());
            for (tid, title) in &req.linked_todos {
                let tid = *tid;
                let tx_open = tx.clone();
                let tx_done = tx.clone();
                let tx_det = tx.clone();
                let short: String = if title.chars().count() > 16 {
                    let mut s: String = title.chars().take(16).collect();
                    s.push('…');
                    s
                } else {
                    title.clone()
                };
                items.push(MenuItem::submenu(
                    format!("已关联 #{tid} {short}"),
                    [
                        MenuItem::new("在看板中查看详情").on_select(move || {
                            emit(&tx_open, TerminalContextAction::OpenTodo(tid));
                        }),
                        MenuItem::new("移至已完成").on_select(move || {
                            emit(&tx_done, TerminalContextAction::CompleteTodo(tid));
                        }),
                        MenuItem::new("解除与当前终端关联").on_select(move || {
                            emit(&tx_det, TerminalContextAction::DetachTodo(tid));
                        }),
                    ],
                ));
            }
        }

        items.push(MenuItem::separator());
        let mut link_sub = Vec::new();
        for (tid, title) in &req.candidate_todos {
            let tid = *tid;
            let tx_l = tx.clone();
            let short: String = if title.chars().count() > 20 {
                let mut s: String = title.chars().take(20).collect();
                s.push('…');
                s
            } else {
                title.clone()
            };
            link_sub.push(MenuItem::new(format!("#{tid} {short}")).on_select(move || {
                emit(&tx_l, TerminalContextAction::LinkTodo(tid));
            }));
        }
        if !link_sub.is_empty() {
            link_sub.push(MenuItem::separator());
        }
        {
            let tx_new = tx.clone();
            link_sub.push(MenuItem::new("新建卡片并关联当前终端").on_select(move || {
                emit(&tx_new, TerminalContextAction::CreateAndLinkTodo);
            }));
        }
        items.push(MenuItem::submenu("关联到 TODO 卡片", link_sub));

        drop(popup_menu_at(
            items,
            Point {
                x: Points(req.x),
                y: Points(req.y),
            },
        ));
    }

    fn panel_transition(opening: bool) -> LayoutTransition {
        let ms = if opening {
            PANEL_OPEN_MS
        } else {
            PANEL_CLOSE_MS
        };
        LayoutTransition::new(LayoutCurve::Bezier(PANEL_EASE), ms)
    }

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct HomeShellRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    #[component]
    pub(super) fn HomeShellRoot(props: HomeShellRootProps) -> impl IntoElement {
        let mount = MOUNTS.take(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (HomeShellSnapshot::default(), None, None),
        };

        let store_error = signal(initial.store_error);
        let sidebar_visible = signal(initial.sidebar_visible);
        let sidebar_width = signal(
            initial
                .sidebar_width
                .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH),
        );
        let plugin_width = signal(
            initial
                .plugin_width
                .clamp(PLUGIN_MIN_WIDTH, PLUGIN_MAX_WIDTH),
        );
        let sidebar_sig = signal(initial.sidebar);
        let tab_bar_sig = signal(initial.tab_bar);
        let todo_board_sig = signal(match &initial.center {
            CenterModeSnapshot::TodoBoard(snap) => Some(snap.clone()),
            _ => None,
        });
        let center_sig = signal(initial.center);
        let pane_signals: PaneSignalMap = Rc::new(RefCell::new(
            initial
                .pane_chrome
                .into_iter()
                .map(|(id, snap)| (id, signal(Some(snap))))
                .collect(),
        ));
        let open_plugin = signal(initial.open_plugin.clone());
        let rendered_plugin = signal(initial.open_plugin);
        let left_ref = element_ref();
        let right_ref = element_ref();
        let anim_gen = stored(0_u64);
        let plugin_close_gen = stored(0_u64);

        let (sidebar_tx, mut sidebar_rx) = unbounded();
        let (tabs_tx, mut tabs_rx) = unbounded();
        let (board_tx, mut board_rx) = unbounded();

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
        let tx_bd = actions_tx.clone();
        spawn(async move {
            while let Some(a) = board_rx.next().await {
                send(&tx_bd, HomeShellAction::TodoBoard(a));
            }
        });

        let pane_signals_ev = pane_signals.clone();
        let tx_ev = actions_tx.clone();
        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(ev) = rx.next().await {
                    match ev {
                        ShellHostEvent::StoreError(err) => store_error.set(err),
                        ShellHostEvent::SidebarVisible(visible) => {
                            if sidebar_visible.get_untracked() != visible {
                                let generation = anim_gen.get().wrapping_add(1);
                                anim_gen.replace(generation);
                                if reduce_motion() {
                                    sidebar_visible.set(visible);
                                    send(&tx_ev, HomeShellAction::PanelAnimationSettled);
                                } else {
                                    let tr = panel_transition(visible);
                                    left_ref.animate_layout(tr);
                                    sidebar_visible.set(visible);
                                    let tx_done = tx_ev.clone();
                                    set_timeout(
                                        Duration::from_millis(
                                            tr.duration_ms as u64 + PANEL_SETTLE_PAD_MS,
                                        ),
                                        move || {
                                            if anim_gen.get() == generation {
                                                send(
                                                    &tx_done,
                                                    HomeShellAction::PanelAnimationSettled,
                                                );
                                            }
                                        },
                                    );
                                }
                            }
                        }
                        ShellHostEvent::Sidebar(s) => sidebar_sig.set(s),
                        ShellHostEvent::TabBar(t) => tab_bar_sig.set(t),
                        ShellHostEvent::Center(c) => match &c {
                            CenterModeSnapshot::TodoBoard(snap) => {
                                todo_board_sig.set(Some(snap.clone()));
                                if !matches!(
                                    center_sig.get_untracked(),
                                    CenterModeSnapshot::TodoBoard(_)
                                ) {
                                    center_sig.set(c);
                                }
                            }
                            _ => {
                                if todo_board_sig.get_untracked().is_some() {
                                    todo_board_sig.set(None);
                                }
                                center_sig.set(c);
                            }
                        },
                        ShellHostEvent::OpenTerminalContextMenu(req) => {
                            open_terminal_popup_menu(req, tx_ev.clone());
                        }
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
                        ShellHostEvent::OpenPlugin(next) => {
                            let prev = open_plugin.get_untracked();
                            if prev != next {
                                match (prev.is_some(), next.is_some()) {
                                    (false, true) => {
                                        plugin_close_gen
                                            .replace(plugin_close_gen.get().wrapping_add(1));
                                        let generation = anim_gen.get().wrapping_add(1);
                                        anim_gen.replace(generation);
                                        if reduce_motion() {
                                            batch(move || {
                                                rendered_plugin.set(next.clone());
                                                open_plugin.set(next);
                                            });
                                            send(&tx_ev, HomeShellAction::PanelAnimationSettled);
                                        } else {
                                            let tr = panel_transition(true);
                                            right_ref.animate_layout(tr);
                                            batch(move || {
                                                rendered_plugin.set(next.clone());
                                                open_plugin.set(next);
                                            });
                                            let tx_done = tx_ev.clone();
                                            set_timeout(
                                                Duration::from_millis(
                                                    tr.duration_ms as u64 + PANEL_SETTLE_PAD_MS,
                                                ),
                                                move || {
                                                    if anim_gen.get() == generation {
                                                        send(
                                                            &tx_done,
                                                            HomeShellAction::PanelAnimationSettled,
                                                        );
                                                    }
                                                },
                                            );
                                        }
                                    }
                                    (true, true) => {
                                        plugin_close_gen
                                            .replace(plugin_close_gen.get().wrapping_add(1));
                                        batch(move || {
                                            rendered_plugin.set(next.clone());
                                            open_plugin.set(next);
                                        });
                                    }
                                    (true, false) => {
                                        let close_gen = plugin_close_gen.get().wrapping_add(1);
                                        plugin_close_gen.replace(close_gen);
                                        let generation = anim_gen.get().wrapping_add(1);
                                        anim_gen.replace(generation);
                                        if reduce_motion() {
                                            batch(move || {
                                                open_plugin.set(None);
                                                rendered_plugin.set(None);
                                            });
                                            send(&tx_ev, HomeShellAction::PluginCloseFinished);
                                            send(&tx_ev, HomeShellAction::PanelAnimationSettled);
                                        } else {
                                            let tr = panel_transition(false);
                                            right_ref.animate_layout(tr);
                                            open_plugin.set(None);
                                            let tx_done = tx_ev.clone();
                                            set_timeout(
                                                Duration::from_millis(
                                                    tr.duration_ms as u64 + PANEL_SETTLE_PAD_MS,
                                                ),
                                                move || {
                                                    if plugin_close_gen.get() == close_gen {
                                                        rendered_plugin.set(None);
                                                        send(
                                                            &tx_done,
                                                            HomeShellAction::PluginCloseFinished,
                                                        );
                                                    }
                                                    if anim_gen.get() == generation {
                                                        send(
                                                            &tx_done,
                                                            HomeShellAction::PanelAnimationSettled,
                                                        );
                                                    }
                                                },
                                            );
                                        }
                                    }
                                    (false, false) => {}
                                }
                            }
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
                .child(kumu_icon_button(
                    "关闭错误提示",
                    icon_svg("icons/close.svg", 12.0).text_color(warning_color),
                    ControlTone::Default,
                    move || send(&tx_x, HomeShellAction::DismissStoreError),
                ))
                .into_element()
        });

        let sidebar_el = render_sidebar(sidebar_sig, Some(sidebar_tx));
        let tab_bar_el = render_tab_bar(tab_bar_sig, Some(tabs_tx));

        let tx_center = actions_tx.clone();
        let pane_signals_center = pane_signals.clone();
        let board_tx_opt = Some(board_tx);
        let center_body = dynamic(move || {
            let tx_c = tx_center.clone();
            let tx_b = tx_center.clone();
            match center_sig.get() {
                CenterModeSnapshot::Panes(root_node) => {
                    render_pane_tree(&root_node, &pane_signals_center, tx_c)
                }
                CenterModeSnapshot::TodoBoard(_) => {
                    render_todo_board(todo_board_sig, board_tx_opt.clone()).into_element()
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
                        view()
                            .flex_row()
                            .gap_2()
                            .child(kumu_button("新建终端", ButtonVariant::Ghost).on_press(
                                move || {
                                    send(&tx_c, HomeShellAction::EmptyAddTerminal);
                                },
                            ))
                            .child(
                                kumu_button("打开 TODO 看板", ButtonVariant::Ghost).on_press(
                                    move || {
                                        send(&tx_b, HomeShellAction::EmptyOpenTodoBoard);
                                    },
                                ),
                            ),
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
            let Some(plugin) = rendered_plugin.get() else {
                return view().size_full().into_element();
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

        // Left sidebar: slides in/out from the left window edge (`justify_end` +
        // fixed inner width keeps content un-squished while the clipped frame
        // animates between `0` and `sidebar_width`).
        let sb_drag_start = stored(0.0_f32);
        let tx_sb_resize = actions_tx.clone();
        let left_sidebar_shell = view()
            .element_ref(&left_ref)
            .relative()
            .flex_row()
            .justify_end()
            .flex_shrink_0()
            .h_full()
            .overflow_hidden()
            .accessibility_elements_hidden(move || !sidebar_visible.get())
            .style_with(move |s| {
                if sidebar_visible.get() {
                    s.w(px(sidebar_width.get()))
                } else {
                    s.w(px(0.0)).pointer_events(PointerEvents::None)
                }
            })
            .child(
                view()
                    .flex_shrink_0()
                    .h_full()
                    .style_with(move |s| s.w(px(sidebar_width.get())))
                    .child(sidebar_el),
            )
            .child(
                resize_handle()
                    .axis(Axis::Horizontal)
                    .edge(HandleEdge::Trailing)
                    .disabled(move || !sidebar_visible.get())
                    .accessibility_label("调整侧边栏宽度")
                    .on_drag_start(move || {
                        sb_drag_start.replace(sidebar_width.get_untracked());
                    })
                    .on_drag_move(move |offset: &Points| {
                        let next = (sb_drag_start.get() + offset.0)
                            .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                        sidebar_width.set(next);
                    })
                    .on_drag_end(move |offset: &Points| {
                        let next = (sb_drag_start.get() + offset.0)
                            .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                        sidebar_width.set(next);
                        send(&tx_sb_resize, HomeShellAction::ResizeSidebar(next));
                    })
                    .child(
                        view()
                            .w(px(1.0))
                            .h_full()
                            .app_class("bg-border group-hover:bg-primary group-active:bg-primary"),
                    ),
            );

        // Right plugin drawer: slides in from the right edge (`justify_start` +
        // fixed inner width) so Git / TODO content glides out intact instead of
        // jumping or squishing.
        let pl_drag_start = stored(0.0_f32);
        let tx_pl_resize = actions_tx.clone();
        let right_plugin_shell =
            view()
                .element_ref(&right_ref)
                .relative()
                .flex_row()
                .justify_start()
                .flex_shrink_0()
                .h_full()
                .overflow_hidden()
                .accessibility_elements_hidden(move || open_plugin.get().is_none())
                .style_with(move |s| {
                    if open_plugin.get().is_some() {
                        s.w(px(plugin_width.get()))
                    } else {
                        s.w(px(0.0)).pointer_events(PointerEvents::None)
                    }
                })
                .child(
                    view()
                        .flex_shrink_0()
                        .h_full()
                        .style_with(move |s| s.w(px(plugin_width.get())))
                        .child(plugin_panel_content),
                )
                .child(when(
                    move || rendered_plugin.get().is_some(),
                    move || {
                        let tx_end = tx_pl_resize.clone();
                        resize_handle()
                            .axis(Axis::Horizontal)
                            .edge(HandleEdge::Leading)
                            .disabled(move || open_plugin.get().is_none())
                            .accessibility_label("调整右侧功能区宽度")
                            .on_drag_start(move || {
                                pl_drag_start.replace(plugin_width.get_untracked());
                            })
                            .on_drag_move(move |offset: &Points| {
                                let next = (pl_drag_start.get() - offset.0)
                                    .clamp(PLUGIN_MIN_WIDTH, PLUGIN_MAX_WIDTH);
                                plugin_width.set(next);
                            })
                            .on_drag_end(move |offset: &Points| {
                                let next = (pl_drag_start.get() - offset.0)
                                    .clamp(PLUGIN_MIN_WIDTH, PLUGIN_MAX_WIDTH);
                                plugin_width.set(next);
                                send(&tx_end, HomeShellAction::ResizePlugin(next));
                            })
                            .child(view().w(px(1.0)).h_full().app_class(
                                "bg-border group-hover:bg-primary group-active:bg-primary",
                            ))
                    },
                ));

        let main_split = view()
            .flex_row()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(left_sidebar_shell)
            .child(
                view()
                    .flex_1()
                    .min_w(px(280.0))
                    .h_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(center_column),
            )
            .child(right_plugin_shell);

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
    }
}

/// Unified Pure Rust NSH home shell handle owned by `KumuApp`.
pub(super) struct NshHomeShell {
    mount_id: u64,
    pub(super) app_view: Entity<AppView>,
    events_tx: UnboundedSender<ShellHostEvent>,
    last_error: Option<String>,
    last_sidebar_visible: bool,
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
        let last_sidebar_visible = initial.sidebar_visible;
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
            last_sidebar_visible,
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
        if self.last_sidebar_visible != next.sidebar_visible {
            self.last_sidebar_visible = next.sidebar_visible;
            let _ = self
                .events_tx
                .unbounded_send(ShellHostEvent::SidebarVisible(next.sidebar_visible));
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

    pub(super) fn open_terminal_context_menu(&self, req: TerminalContextMenuRequest) {
        let _ = self
            .events_tx
            .unbounded_send(ShellHostEvent::OpenTerminalContextMenu(req));
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
                agent_unseen: false,
                active: true,
                can_close: true,
                can_zoom: true,
                zoomed: false,
                find: Some(FindBarSnapshot {
                    needle: "hello".into(),
                    count_label: "1/3".into(),
                    has_matches: true,
                    focus_nonce: 1,
                }),
                linked_todos: Vec::new(),
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
        let mut config = EngineConfig::new(root);
        config.clock = nsh_core::time::Clock::manual(std::time::SystemTime::UNIX_EPOCH);
        let mut app = nsh_testing::Headless::start(config).expect("start unified home shell");
        app.set_viewport(nsh_core::Size::new(
            nsh_core::Points(1200.0),
            nsh_core::Points(800.0),
        ))
        .expect("set desktop viewport");

        app.wait_for("zsh").expect("pane header name renders");
        app.wait_for("1/3").expect("find bar match count renders");

        app.press("label:新建 Agent 窗格")
            .expect("press accessible Agent picker button");
        match actions_rx.try_recv().expect("open agent picker action") {
            HomeShellAction::OpenAgentPicker { pane, anchor } => {
                assert_eq!(pane, PaneId(1));
                assert!(anchor.x > 0.0);
                assert!(anchor.y > 0.0);
            }
            action => panic!("unexpected shell action: {action:?}"),
        }

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
                agent_unseen: false,
                active: true,
                can_close: true,
                can_zoom: true,
                zoomed: false,
                find: Some(FindBarSnapshot {
                    needle: "hello".into(),
                    count_label: "2/3".into(),
                    has_matches: true,
                    focus_nonce: 2,
                }),
                linked_todos: Vec::new(),
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

        app.press("label:最大化窗格 ⌘⇧↩")
            .expect("press maximize pane");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(HomeShellAction::ToggleZoomPane(PaneId(1)))
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

    #[test]
    fn drawer_transitions_settle_and_preserve_content_while_closing() {
        let (events_tx, events_rx) = unbounded::<ShellHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<HomeShellAction>();
        let initial = HomeShellSnapshot {
            sidebar_visible: true,
            sidebar_width: 256.0,
            plugin_width: 300.0,
            center: CenterModeSnapshot::Panes(LayoutNodeSnapshot::Leaf(PaneId(1))),
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
        let mut config = EngineConfig::new(root);
        config.clock = nsh_core::time::Clock::manual(std::time::SystemTime::UNIX_EPOCH);
        let mut app = nsh_testing::Headless::start(config).expect("start unified home shell");
        app.set_viewport(nsh_core::Size::new(
            nsh_core::Points(1200.0),
            nsh_core::Points(800.0),
        ))
        .expect("set desktop viewport");

        let drain_actions =
            |rx: &mut futures::channel::mpsc::UnboundedReceiver<HomeShellAction>| {
                let mut out = Vec::new();
                while let Ok(a) = rx.try_recv() {
                    out.push(a);
                }
                out
            };

        // 1. Opening right plugin drawer renders its header and settles after animation.
        events_tx
            .unbounded_send(ShellHostEvent::OpenPlugin(Some(PluginPanelSnapshot {
                id: "git_log",
                title: "Git",
                icon: "icons/git-branch.svg",
            })))
            .expect("open git plugin");
        app.wait_for("Git").expect("git header renders");
        app.fast_forward(std::time::Duration::from_millis(300))
            .expect("advance open animation");
        let opened_actions = drain_actions(&mut actions_rx);
        assert!(
            opened_actions.contains(&HomeShellAction::PanelAnimationSettled),
            "expected PanelAnimationSettled after opening drawer, got {opened_actions:?}"
        );

        // 2. Switching directly to TODO swaps content in place without closing.
        events_tx
            .unbounded_send(ShellHostEvent::OpenPlugin(Some(PluginPanelSnapshot {
                id: "todo",
                title: "TODO",
                icon: "icons/list-todo.svg",
            })))
            .expect("switch to todo plugin");
        app.wait_for("TODO").expect("todo header renders");
        let switched_actions = drain_actions(&mut actions_rx);
        assert!(
            !switched_actions.contains(&HomeShellAction::PluginCloseFinished),
            "switching plugins should not trigger PluginCloseFinished: {switched_actions:?}"
        );

        // 3. Closing right drawer keeps content mounted during the slide-out, then
        // emits PluginCloseFinished + PanelAnimationSettled once settled.
        events_tx
            .unbounded_send(ShellHostEvent::OpenPlugin(None))
            .expect("close right plugin");
        app.run_for(std::time::Duration::from_millis(50))
            .expect("mid-close frame");
        let mid_close_actions = drain_actions(&mut actions_rx);
        assert!(
            !mid_close_actions.contains(&HomeShellAction::PluginCloseFinished),
            "plugin must stay mounted while sliding out: {mid_close_actions:?}"
        );
        app.run_for(std::time::Duration::from_millis(250))
            .expect("finish close animation");
        let closed_actions = drain_actions(&mut actions_rx);
        assert!(
            closed_actions.contains(&HomeShellAction::PluginCloseFinished),
            "expected PluginCloseFinished after close animation, got {closed_actions:?}"
        );
        assert!(
            closed_actions.contains(&HomeShellAction::PanelAnimationSettled),
            "expected PanelAnimationSettled after close animation, got {closed_actions:?}"
        );

        // 4. Toggling left sidebar also emits PanelAnimationSettled once settled.
        events_tx
            .unbounded_send(ShellHostEvent::SidebarVisible(false))
            .expect("collapse left sidebar");
        app.run_for(std::time::Duration::from_millis(260))
            .expect("finish sidebar collapse");
        let sidebar_actions = drain_actions(&mut actions_rx);
        assert!(
            sidebar_actions.contains(&HomeShellAction::PanelAnimationSettled),
            "expected PanelAnimationSettled after collapsing sidebar, got {sidebar_actions:?}"
        );
    }
}
