//! Pure Rust NSH UI for the project-level Tab Bar above the terminal pane grid.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::component::Theme;
use gpui_kit::{AppContext, Context, Entity, Task, Window};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use crate::app::layout::TabId;
use crate::nsh_ui::theme::{appearance, nsh_theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabAttention {
    Waiting,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabItemSnapshot {
    pub(super) id: TabId,
    pub(super) name: String,
    pub(super) active: bool,
    pub(super) attention: Option<TabAttention>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum TabBarSnapshot {
    #[default]
    Hidden,
    ConfirmClose {
        project_id: i64,
        tab_id: TabId,
        name: String,
        pane_count: usize,
    },
    Tabs {
        project_id: i64,
        selected_tab_id: Option<TabId>,
        tabs: Vec<TabItemSnapshot>,
    },
}

#[derive(Clone, Debug)]
pub(super) enum TabBarHostEvent {
    Snapshot(TabBarSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TabBarAction {
    Select { project_id: i64, tab_id: TabId },
    RequestClose { project_id: i64, tab_id: TabId },
    CancelClose,
    ConfirmClose { project_id: i64, tab_id: TabId },
    Add,
    PromptRename { project_id: i64, tab_id: TabId },
}

struct TabBarMount {
    initial: TabBarSnapshot,
    events_rx: UnboundedReceiver<TabBarHostEvent>,
    actions_tx: UnboundedSender<TabBarAction>,
}

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, TabBarMount>>> = OnceLock::new();

fn pending_mounts() -> &'static Mutex<HashMap<u64, TabBarMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_mount(mount: TabBarMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

fn take_mount(id: u64) -> Option<TabBarMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

mod view {
    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::theme as active_theme;
    use nsh_core::element::desktop::{MenuItem, popup_menu};
    use nsh_core::element::{AccessibilityRole, scroll};
    use nsh_core::prelude::*;

    use super::{TabAttention, TabBarAction, TabBarHostEvent, TabBarSnapshot, take_mount};
    use crate::nsh_ui::assets::icon_svg;
    use crate::nsh_ui::components::{ButtonVariant, kumu_button};
    use crate::nsh_ui::theme::{TOKEN_WARNING, kumu_color, text_xs_size};

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct TabBarRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    fn send(tx: &Option<UnboundedSender<TabBarAction>>, action: TabBarAction) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(action);
        }
    }

    fn warning_color() -> AdaptiveColor {
        kumu_color(TOKEN_WARNING, |t| t.destructive)
    }

    #[component]
    pub(super) fn TabBarRoot(props: TabBarRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (TabBarSnapshot::Hidden, None, None),
        };

        let state = signal(initial);

        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        TabBarHostEvent::Snapshot(next) => state.set(next),
                    }
                }
            });
        }

        let tx_root = actions_tx.clone();
        dynamic(move || match state.get() {
            TabBarSnapshot::Hidden => view().w_full().h_full().into_element(),
            TabBarSnapshot::ConfirmClose {
                project_id,
                tab_id,
                name,
                pane_count,
            } => {
                let tx_cancel = tx_root.clone();
                let tx_confirm = tx_root.clone();
                view()
                    .flex_row()
                    .w_full()
                    .h_full()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .bg(move || warning_color().opacity(0.12))
                    .border_b_1()
                    .border_b_color(move || active_theme().border)
                    .font_family(move || {
                        active_theme()
                            .font_sans
                            .clone()
                            .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
                    })
                    .child(
                        text(format!("关闭「{name}」会结束 {pane_count} 个终端。"))
                            .flex_1()
                            .min_w_0()
                            .text_size(text_xs_size)
                            .text_color(warning_color),
                    )
                    .child(
                        kumu_button("取消", ButtonVariant::Ghost)
                            .on_press(move || send(&tx_cancel, TabBarAction::CancelClose)),
                    )
                    .child(
                        kumu_button("关闭页签", ButtonVariant::Default).on_press(move || {
                            send(
                                &tx_confirm,
                                TabBarAction::ConfirmClose { project_id, tab_id },
                            );
                        }),
                    )
                    .into_element()
            }
            TabBarSnapshot::Tabs {
                project_id,
                selected_tab_id,
                tabs,
            } => {
                let mut strip = view().flex_row().gap_1().items_center();
                for tab in tabs {
                    let tab_id = tab.id;
                    let active = tab.active;
                    let tx_sel = tx_root.clone();
                    let tx_close = tx_root.clone();

                    let mut tab_el = view()
                        .role(AccessibilityRole::Button)
                        .flex_row()
                        .flex_shrink_0()
                        .items_center()
                        .gap_1()
                        .h(px(26.0))
                        .px_2()
                        .rounded(move || active_theme().radius)
                        .on_press(move || {
                            send(&tx_sel, TabBarAction::Select { project_id, tab_id });
                        })
                        .child(
                            icon_svg("icons/layout-dashboard.svg", 14.0)
                                .text_color(move || active_theme().muted_foreground),
                        )
                        .child(
                            text(tab.name)
                                .max_w(px(160.0))
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || {
                                    if active {
                                        active_theme().foreground
                                    } else {
                                        active_theme().muted_foreground
                                    }
                                }),
                        );

                    if active {
                        tab_el = tab_el
                            .bg(move || active_theme().background)
                            .border_1()
                            .border_color(move || active_theme().border);
                    } else {
                        tab_el = tab_el.hover(|s| s.bg(move || active_theme().accent.opacity(0.5)));
                    }

                    if let Some(att) = tab.attention {
                        tab_el = tab_el.child(match att {
                            TabAttention::Waiting => view()
                                .w(px(7.0))
                                .h(px(7.0))
                                .flex_shrink_0()
                                .rounded(px(3.5))
                                .bg(warning_color)
                                .into_element(),
                            TabAttention::Failed => icon_svg("icons/triangle-alert.svg", 12.0)
                                .text_color(move || active_theme().destructive)
                                .into_element(),
                        });
                    }

                    tab_el = tab_el.child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("关闭页签")
                            .flex_shrink_0()
                            .rounded(move || active_theme().radius)
                            .p_0p5()
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                            .on_press(move || {
                                send(&tx_close, TabBarAction::RequestClose { project_id, tab_id });
                            })
                            .child(
                                icon_svg("icons/x.svg", 12.0)
                                    .opacity(if active { 1.0 } else { 0.55 })
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    );

                    strip = strip.child(tab_el);
                }

                let tx_add = tx_root.clone();
                let tx_menu = tx_root.clone();

                view()
                    .flex_row()
                    .w_full()
                    .h_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .bg(move || active_theme().secondary.opacity(0.45))
                    .border_b_1()
                    .border_b_color(move || active_theme().border)
                    .font_family(move || {
                        active_theme()
                            .font_sans
                            .clone()
                            .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
                    })
                    .child(scroll().horizontal().flex_1().min_w_0().child(strip))
                    .child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("新建页签")
                            .flex_shrink_0()
                            .rounded(move || active_theme().radius)
                            .p_0p5()
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                            .on_press(move || send(&tx_add, TabBarAction::Add))
                            .child(
                                icon_svg("icons/plus.svg", 14.0)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    )
                    .child(
                        view()
                            .role(AccessibilityRole::Button)
                            .label("页签操作")
                            .flex_shrink_0()
                            .rounded(move || active_theme().radius)
                            .p_0p5()
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.5)))
                            .on_press(move || {
                                let Some(tab_id) = selected_tab_id else {
                                    return;
                                };
                                let tx_r = tx_menu.clone();
                                let tx_c = tx_menu.clone();
                                drop(popup_menu(vec![
                                    MenuItem::new("重命名页签").on_select(move || {
                                        send(
                                            &tx_r,
                                            TabBarAction::PromptRename { project_id, tab_id },
                                        );
                                    }),
                                    MenuItem::new("关闭页签").on_select(move || {
                                        send(
                                            &tx_c,
                                            TabBarAction::RequestClose { project_id, tab_id },
                                        );
                                    }),
                                ]));
                            })
                            .child(
                                icon_svg("icons/ellipsis.svg", 14.0)
                                    .text_color(move || active_theme().muted_foreground),
                            ),
                    )
                    .into_element()
            }
        })
    }
}

/// Host handle for the Pure Rust NSH tab strip owned by `KumuApp`.
pub(super) struct NshTabBar {
    mount_id: u64,
    pub(super) app_view: Entity<AppView>,
    events_tx: UnboundedSender<TabBarHostEvent>,
    last_snapshot: TabBarSnapshot,
    _actions_task: Task<()>,
}

impl Drop for NshTabBar {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl NshTabBar {
    pub(super) fn new<V: 'static>(
        initial: TabBarSnapshot,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_action: impl FnMut(&mut V, TabBarAction, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (events_tx, events_rx) = unbounded::<TabBarHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<TabBarAction>();
        let mount_id = register_mount(TabBarMount {
            initial: initial.clone(),
            events_rx,
            actions_tx,
        });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Tabs".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<view::TabBarRoot>(
                move || view::TabBarRootProps {
                    mount_id: mount_id.into(),
                },
            )));
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
            last_snapshot: initial,
            _actions_task: actions_task,
        }
    }

    pub(super) fn sync(&mut self, snapshot: TabBarSnapshot) {
        if self.last_snapshot != snapshot {
            self.last_snapshot = snapshot.clone();
            let _ = self
                .events_tx
                .unbounded_send(TabBarHostEvent::Snapshot(snapshot));
        }
    }

    pub(super) fn sync_theme(&self, theme: &Theme, cx: &mut gpui_kit::App) {
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

    #[test]
    fn tab_bar_nsh_selects_tabs_and_confirms_multi_pane_close() {
        let (events_tx, events_rx) = unbounded::<TabBarHostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<TabBarAction>();
        let initial = TabBarSnapshot::Tabs {
            project_id: 1,
            selected_tab_id: Some(TabId(1)),
            tabs: vec![
                TabItemSnapshot {
                    id: TabId(1),
                    name: "工作区 1".into(),
                    active: true,
                    attention: None,
                },
                TabItemSnapshot {
                    id: TabId(2),
                    name: "工作区 2".into(),
                    active: false,
                    attention: Some(TabAttention::Waiting),
                },
            ],
        };
        let mount_id = register_mount(TabBarMount {
            initial,
            events_rx,
            actions_tx,
        });
        let root =
            nsh_core::element::Root::new::<view::TabBarRoot>(move || view::TabBarRootProps {
                mount_id: mount_id.into(),
            });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start tabs NSH");
        app.wait_for("工作区 2").expect("tab 2 renders");
        app.press("工作区 2").expect("click tab 2");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(TabBarAction::Select {
                project_id: 1,
                tab_id: TabId(2),
            })
        );

        events_tx
            .unbounded_send(TabBarHostEvent::Snapshot(TabBarSnapshot::ConfirmClose {
                project_id: 1,
                tab_id: TabId(2),
                name: "工作区 2".into(),
                pane_count: 3,
            }))
            .expect("push confirm banner");
        app.wait_for("关闭「工作区 2」会结束 3 个终端。")
            .expect("confirm banner renders");
        app.press("关闭页签").expect("confirm close");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(TabBarAction::ConfirmClose {
                project_id: 1,
                tab_id: TabId(2),
            })
        );
    }
}
