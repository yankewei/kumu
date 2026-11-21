//! Settings window rendered in Pure Rust NSH (`nsh-platform-gpui`),
//! Waku-style: sidebar (back row + search + icon nav) on the left; a centered
//! scrollable column with card-grouped rows on the right.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Task,
    Window, div,
};
use gpui_kit::component::{StyledExt, TitleBar};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::theme::{appearance, nsh_theme};
use super::toast::ToastItem;
use crate::agent::{AGENTS, integration};
use crate::app::NotificationSettings;
use crate::db;
use crate::theme::{self, KumuTheme, ThemeMode};
use crate::updates::{ErrorKind, UpdateStatus, Updater};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsPage {
    Providers,
    General,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentStatusRow {
    installed: bool,
    status: Option<Result<integration::Status, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateSnapshot {
    message: String,
    button_label: &'static str,
    busy: bool,
    is_staged: bool,
}

impl UpdateSnapshot {
    fn from_status(status: &UpdateStatus) -> Self {
        let (message, button_label) = match status {
            UpdateStatus::Idle => ("启动时自动检查更新。".to_string(), "检查更新"),
            UpdateStatus::Checking => ("正在检查新版本…".to_string(), "检查中"),
            UpdateStatus::UpToDate => ("当前已是最新版本。".to_string(), "检查更新"),
            UpdateStatus::Available(version) => (format!("发现新版本 {version}。"), "下载并安装"),
            UpdateStatus::Downloading { downloaded, total } => {
                let message = match total {
                    Some(total) if *total > 0 => format!(
                        "正在下载：{}%",
                        (downloaded.saturating_mul(100) / total).min(100)
                    ),
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
        Self {
            message,
            button_label,
            busy: matches!(status, UpdateStatus::Checking | UpdateStatus::Installing),
            is_staged: matches!(status, UpdateStatus::Staged(_)),
        }
    }
}

/// Snapshot of host-owned state pushed to the NSH Settings view.
#[derive(Clone, Debug)]
struct InitialSettingsState {
    dark: bool,
    font_family: String,
    font_size: u32,
    system_fonts: Vec<(String, String)>,
    notifications_enabled: bool,
    notification_sound: bool,
    auto_resume: bool,
    updater: UpdateSnapshot,
}

/// Events pushed from the GPUI host into the NSH Settings engine thread.
#[derive(Clone, Debug)]
enum HostEvent {
    ThemeChanged {
        dark: bool,
        font_family: String,
        font_size: u32,
    },
    UpdaterStatus(UpdateSnapshot),
    AgentsProbed(Vec<(&'static str, bool, Result<integration::Status, String>)>),
    IntegrationBusy(Option<&'static str>),
    IntegrationFinished {
        agent: &'static str,
        status: Result<integration::Status, String>,
    },
    NotificationSettings {
        enabled: bool,
        sound: bool,
    },
    AutoResume(bool),
    ShowUpdates {
        prompt_restart: bool,
    },
    Toast(ToastItem),
}

/// Actions sent from the NSH Settings UI back to the GPUI host.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SettingsAction {
    CloseWindow,
    SetDarkMode(bool),
    SetFontFamily(String),
    SetFontSize(u32),
    SetNotifications(bool),
    SetNotificationSound(bool),
    SetAutoResume(bool),
    ToggleIntegration { agent: &'static str, remove: bool },
    UpdatePrimaryAction,
    ConfirmRestartUpdate,
    OpenReleasePage,
}

/// Per-window mount payload moved once into the NSH root component via a
/// unique mount ID (removed on mount or window drop — zero leaks across
/// repeated open/close cycles).
struct SettingsMount {
    initial: InitialSettingsState,
    events_rx: UnboundedReceiver<HostEvent>,
    actions_tx: UnboundedSender<SettingsAction>,
}

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, SettingsMount>>> = OnceLock::new();

fn pending_mounts() -> &'static Mutex<HashMap<u64, SettingsMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_mount(mount: SettingsMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

fn take_mount(id: u64) -> Option<SettingsMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

mod view {
    use std::collections::HashMap;

    use futures::StreamExt;
    use futures::channel::mpsc::UnboundedSender;
    use nsh_base::{
        SelectEntry, SelectItem, SelectValue, dialog, input, select, theme as active_theme,
    };
    use nsh_core::element::{AccessibilityRole, scroll};
    use nsh_core::prelude::*;

    use super::{
        AgentStatusRow, HostEvent, SettingsAction, SettingsPage, UpdateSnapshot, take_mount,
    };
    use crate::agent::{AGENTS, integration};
    use crate::nsh_ui::assets::{agent_logo, icon_svg};
    use crate::nsh_ui::components::{
        ButtonVariant, PillTone, kumu_button, kumu_switch_bare, meta_text, pill, row_title,
        section_card, section_title,
    };
    use crate::nsh_ui::theme::{TOKEN_ICON_TILE, kumu_color, text_xs_size};

    #[derive(Clone, Debug, PartialEq, Props)]
    pub struct SettingsRootProps {
        pub mount_id: Prop<u64>,
    }

    fn send_action(tx: &Option<UnboundedSender<SettingsAction>>, action: SettingsAction) {
        if let Some(tx) = tx {
            let _ = tx.unbounded_send(action);
        }
    }

    #[component]
    pub fn SettingsRoot(props: SettingsRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());

        let (initial, events_rx, actions_tx) = match mount {
            Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
            None => (
                super::InitialSettingsState {
                    dark: false,
                    font_family: crate::theme::SYSTEM_FONT_FAMILY.to_string(),
                    font_size: crate::theme::DEFAULT_FONT_SIZE as u32,
                    system_fonts: vec![(
                        crate::theme::SYSTEM_FONT_LABEL.to_string(),
                        crate::theme::SYSTEM_FONT_FAMILY.to_string(),
                    )],
                    notifications_enabled: true,
                    notification_sound: true,
                    auto_resume: true,
                    updater: UpdateSnapshot::from_status(&crate::updates::UpdateStatus::Idle),
                },
                None,
                None,
            ),
        };

        let page = signal(SettingsPage::Providers);
        let search_query = signal(String::new());
        let dark = signal(initial.dark);
        let font_family = signal(initial.font_family);
        let font_size = signal(initial.font_size);
        let notifications_enabled = signal(initial.notifications_enabled);
        let notification_sound = signal(initial.notification_sound);
        let auto_resume = signal(initial.auto_resume);
        let updater = signal(initial.updater);
        let agents_state = signal(HashMap::<&'static str, AgentStatusRow>::new());
        let integration_busy = signal(Option::<&'static str>::None);
        let confirm_restart_open = signal(false);
        let (toast_tx, toast_rx) = futures::channel::mpsc::unbounded();

        let content_scroll = element_ref();

        let font_items: Vec<SelectItem> = initial
            .system_fonts
            .into_iter()
            .map(|(label, family)| SelectItem::new(family, label))
            .collect();
        let font_label_map: HashMap<String, String> = font_items
            .iter()
            .map(|item| (item.value.to_string(), item.title.to_string()))
            .collect();
        let font_size_items: Vec<SelectItem> = crate::theme::FONT_SIZES
            .into_iter()
            .map(|sz| {
                let s = sz.to_string();
                SelectItem::new(s.clone(), s)
            })
            .collect();

        if let Some(mut rx) = events_rx {
            spawn(async move {
                while let Some(event) = rx.next().await {
                    match event {
                        HostEvent::ThemeChanged {
                            dark: is_dark,
                            font_family: fam,
                            font_size: sz,
                        } => {
                            dark.set(is_dark);
                            font_family.set(fam);
                            font_size.set(sz);
                        }
                        HostEvent::UpdaterStatus(snapshot) => {
                            updater.set(snapshot);
                        }
                        HostEvent::AgentsProbed(results) => {
                            agents_state.update(|map| {
                                for (id, installed, status) in results {
                                    map.insert(
                                        id,
                                        AgentStatusRow {
                                            installed,
                                            status: Some(status),
                                        },
                                    );
                                }
                            });
                        }
                        HostEvent::IntegrationBusy(busy) => {
                            integration_busy.set(busy);
                        }
                        HostEvent::IntegrationFinished { agent, status } => {
                            integration_busy.set(None);
                            agents_state.update(|map| {
                                let entry = map.entry(agent).or_insert(AgentStatusRow {
                                    installed: true,
                                    status: None,
                                });
                                entry.status = Some(status);
                            });
                        }
                        HostEvent::NotificationSettings { enabled, sound } => {
                            notifications_enabled.set(enabled);
                            notification_sound.set(sound);
                        }
                        HostEvent::AutoResume(enabled) => {
                            auto_resume.set(enabled);
                        }
                        HostEvent::ShowUpdates { prompt_restart } => {
                            page.set(SettingsPage::General);
                            search_query.set(String::new());
                            content_scroll.scroll_to_end(false);
                            if prompt_restart {
                                confirm_restart_open.set(true);
                            }
                        }
                        HostEvent::Toast(item) => {
                            let _ = toast_tx
                                .unbounded_send(crate::nsh_ui::toast::ToastCommand::Push(item));
                        }
                    }
                }
            });
        }

        // ---- Left sidebar: back row + search + icon nav ----
        let tx_back = actions_tx.clone();
        let back_row = view()
            .role(AccessibilityRole::Button)
            .h(px(36.0))
            .px_2p5()
            .rounded(px(8.0))
            .flex_row()
            .items_center()
            .gap_2p5()
            .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
            .on_press(move || send_action(&tx_back, SettingsAction::CloseWindow))
            .child(
                icon_svg("icons/arrow-left.svg", 14.0)
                    .text_color(move || active_theme().muted_foreground),
            )
            .child(
                text("返回")
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().sidebar_foreground),
            );

        let search_box = view()
            .flex_row()
            .items_center()
            .gap_1p5()
            .px_2()
            .h(px(28.0))
            .rounded(move || active_theme().radius)
            .border_1()
            .border_color(move || active_theme().input)
            .bg(move || active_theme().background)
            .child(
                icon_svg("icons/search.svg", 13.0)
                    .text_color(move || active_theme().muted_foreground),
            )
            .child(
                input()
                    .flex_1()
                    .placeholder("搜索设置…")
                    .placeholder_color(move || active_theme().muted_foreground.light)
                    .text_size(text_xs_size)
                    .text_color(move || active_theme().foreground)
                    .value(move || search_query.get())
                    .on_change(move |val: &str| search_query.set(val.to_owned())),
            );

        let nav_section = dynamic(move || {
            let query = search_query.get().to_lowercase();
            let current_page = page.get();
            let nav_defs = [
                ("icons/bot.svg", "服务商", SettingsPage::Providers),
                ("icons/settings.svg", "通用", SettingsPage::General),
            ];
            let mut list = view().flex_col().px_2().gap_0p5();
            for (icon_path, label, target) in nav_defs {
                if !query.is_empty() && !label.to_lowercase().contains(&query) {
                    continue;
                }
                let active = current_page == target;
                let mut item = view()
                    .role(AccessibilityRole::Button)
                    .h(px(36.0))
                    .px_2p5()
                    .rounded(px(8.0))
                    .flex_row()
                    .items_center()
                    .gap_2p5()
                    .on_press(move || {
                        page.set(target);
                        content_scroll.scroll_to_offset(px(0.0), false);
                    });
                if active {
                    item = item.bg(move || active_theme().sidebar_accent);
                } else {
                    item = item.hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)));
                }
                item = item
                    .child(icon_svg(icon_path, 15.0).text_color(move || {
                        if active {
                            active_theme().sidebar_accent_foreground
                        } else {
                            active_theme().muted_foreground
                        }
                    }))
                    .child(text(label).text_size(text_xs_size).text_color(move || {
                        if active {
                            active_theme().sidebar_accent_foreground
                        } else {
                            active_theme().sidebar_foreground
                        }
                    }));
                list = list.child(item);
            }
            list
        });

        let sidebar = view()
            .flex_col()
            .w(px(224.0))
            .h_full()
            .flex_shrink_0()
            .bg(move || active_theme().sidebar)
            .border_r_1()
            .border_r_color(move || active_theme().sidebar_border)
            .child(view().px_2().pt_3().child(back_row))
            .child(view().px_2().pt_3().child(search_box))
            .child(view().h(px(8.0)))
            .child(nav_section);

        // ---- Right scrollable content column ----
        let tx_providers = actions_tx.clone();
        let tx_general = actions_tx.clone();
        let tx_updates = actions_tx.clone();

        let providers_card = move || {
            let tx_providers = tx_providers.clone();
            section_card()
                .child(
                    view()
                        .flex_col()
                        .gap_1()
                        .child(section_title("编码 Agent"))
                        .child(meta_text(
                            "开启通知会注册到 Agent 的配置或扩展目录，之后可按原来的方式启动。",
                        )),
                )
                .child(dynamic(move || {
                    let query = search_query.get().to_lowercase();
                    let states = agents_state.get();
                    let busy_agent = integration_busy.get();
                    let filtered: Vec<_> = AGENTS
                        .iter()
                        .filter(|a| {
                            query.is_empty()
                                || a.name.to_lowercase().contains(&query)
                                || a.id.to_lowercase().contains(&query)
                        })
                        .collect();
                    let total = filtered.len();
                    let mut rows = view().flex_col().mt_1();
                    for (idx, a) in filtered.into_iter().enumerate() {
                        let row_state = states.get(a.id);
                        let installed = row_state.is_some_and(|r| r.installed);
                        let status = row_state.and_then(|r| r.status.as_ref());
                        let registered =
                            matches!(status, Some(Ok(integration::Status::Registered)));
                        let needs_repair =
                            matches!(status, Some(Ok(integration::Status::NeedsRepair)));

                        let notification_pill = if installed && registered {
                            Some((PillTone::Accent, "已开启"))
                        } else if installed && needs_repair {
                            Some((PillTone::Neutral, "需更新"))
                        } else {
                            None
                        };
                        let hint = (installed && (registered || needs_repair) && a.id == "codex")
                            .then_some("在 Codex /hooks 中确认信任");
                        let (action_label, removing) = if registered {
                            ("关闭通知", true)
                        } else if needs_repair {
                            ("更新配置", false)
                        } else {
                            ("开启通知", false)
                        };

                        let mut logo_wrap = view().child(agent_logo(a.logo, 16.0));
                        if !installed {
                            logo_wrap = logo_wrap.opacity(0.45);
                        }

                        let icon_tile = view()
                            .w(px(30.0))
                            .h(px(30.0))
                            .flex_shrink_0()
                            .rounded(move || active_theme().radius)
                            .bg(move || kumu_color(TOKEN_ICON_TILE, |t| t.muted))
                            .items_center()
                            .justify_center()
                            .child(logo_wrap);

                        let mut header_line = view().flex_row().items_center().gap_1p5().child(
                            text(a.name)
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(move || {
                                    if installed {
                                        active_theme().foreground
                                    } else {
                                        active_theme().muted_foreground
                                    }
                                }),
                        );
                        if !installed {
                            header_line = header_line.child(meta_text("· 未安装"));
                        }
                        if let Some((tone, pill_label)) = notification_pill {
                            header_line = header_line.child(pill(pill_label, tone));
                        }

                        let mut info_col = view()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(header_line)
                            .child(view().mt_0p5().child(meta_text(a.desc)));
                        if let Some(hint_text) = hint {
                            info_col = info_col.child(view().mt_0p5().child(meta_text(hint_text)));
                        }

                        let mut row_el = view()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .py_3()
                            .child(icon_tile)
                            .child(info_col);

                        if idx + 1 < total {
                            row_el = row_el
                                .border_b_1()
                                .border_b_color(move || active_theme().border);
                        }

                        if installed {
                            let is_this_busy = busy_agent == Some(a.id);
                            let any_busy = busy_agent.is_some();
                            let tx = tx_providers.clone();
                            let agent_id = a.id;
                            let btn_text = if is_this_busy {
                                "处理中…"
                            } else {
                                action_label
                            };
                            let variant = if removing {
                                ButtonVariant::Ghost
                            } else {
                                ButtonVariant::Outline
                            };
                            row_el = row_el.child(
                                kumu_button(btn_text, variant).disabled(any_busy).on_press(
                                    move || {
                                        send_action(
                                            &tx,
                                            SettingsAction::ToggleIntegration {
                                                agent: agent_id,
                                                remove: removing,
                                            },
                                        );
                                    },
                                ),
                            );
                        }
                        rows = rows.child(row_el);
                    }
                    rows
                }))
        };

        let general_cards = move || {
            let tx_dark = tx_general.clone();
            let tx_font = tx_general.clone();
            let tx_size = tx_general.clone();
            let tx_notif = tx_general.clone();
            let tx_sound = tx_general.clone();
            let tx_resume = tx_general.clone();
            let tx_upd_btn = tx_updates.clone();
            let tx_release = tx_updates.clone();

            let font_items = font_items.clone();
            let font_label_map = font_label_map.clone();
            let font_size_items = font_size_items.clone();

            let settings_row =
                |title: &'static str, desc: &'static str, control: AnyElement, last: bool| {
                    let mut r = view()
                        .flex_row()
                        .items_center()
                        .gap_6()
                        .py_3()
                        .child(
                            view()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(row_title(title))
                                .child(view().mt_0p5().child(meta_text(desc))),
                        )
                        .child(control);
                    if !last {
                        r = r.border_b_1().border_b_color(move || active_theme().border);
                    }
                    r
                };

            let dark_switch = kumu_switch_bare(move || dark.get())
                .accessibility_label("深色外观")
                .on_change(move |checked: &bool| {
                    dark.set(*checked);
                    send_action(&tx_dark, SettingsAction::SetDarkMode(*checked));
                })
                .into_element();

            let font_picker = {
                let labels = font_label_map.clone();
                let trigger = view()
                    .w(px(192.0))
                    .h(px(28.0))
                    .px_2p5()
                    .rounded(move || active_theme().radius)
                    .border_1()
                    .border_color(move || active_theme().input)
                    .bg(move || active_theme().background)
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        text(move || {
                            let fam = font_family.get();
                            labels.get(&fam).cloned().unwrap_or(fam)
                        })
                        .flex_1()
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground),
                    )
                    .child(
                        icon_svg("icons/chevron-down.svg", 13.0)
                            .text_color(move || active_theme().muted_foreground),
                    );

                select()
                    .items(font_items)
                    .value(move || SelectValue::from(font_family.get()))
                    .searchable(true)
                    .search_placeholder("搜索字体…")
                    .placeholder("选择字体")
                    .accessibility_label("界面字体")
                    .cover(false)
                    .offset(px(4.0))
                    .popup_class(
                        "bg-popover border border-border rounded-lg shadow-md overflow-hidden p-1",
                    )
                    .search_class(
                        "w-full px-2 py-1 mb-1 text-xs text-foreground bg-background border border-input rounded",
                    )
                    .list_class("max-h-60")
                    .item_class(
                        "px-2.5 py-1.5 rounded text-xs text-popover-foreground focus:bg-accent selected:font-medium",
                    )
                    .render_item(|entry: &SelectEntry| {
                        text(entry.title.clone())
                            .font_family(entry.value.clone())
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().popover_foreground)
                    })
                    .trigger(trigger)
                    .on_change(move |chosen: &Option<SharedString>| {
                        if let Some(family) = chosen {
                            let fam = family.to_string();
                            font_family.set(fam.clone());
                            send_action(&tx_font, SettingsAction::SetFontFamily(fam));
                        }
                    })
                    .into_element()
            };

            let font_size_picker = {
                let trigger = view()
                    .w(px(192.0))
                    .h(px(28.0))
                    .px_2p5()
                    .rounded(move || active_theme().radius)
                    .border_1()
                    .border_color(move || active_theme().input)
                    .bg(move || active_theme().background)
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        text(move || font_size.get().to_string())
                            .flex_1()
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().foreground),
                    )
                    .child(
                        icon_svg("icons/chevron-down.svg", 13.0)
                            .text_color(move || active_theme().muted_foreground),
                    );

                select()
                    .items(font_size_items)
                    .value(move || SelectValue::from(font_size.get().to_string()))
                    .placeholder("选择字号")
                    .accessibility_label("界面字体大小")
                    .cover(false)
                    .offset(px(4.0))
                    .popup_class(
                        "bg-popover border border-border rounded-lg shadow-md overflow-hidden p-1",
                    )
                    .list_class("max-h-60")
                    .item_class(
                        "px-2.5 py-1.5 rounded text-xs text-popover-foreground focus:bg-accent selected:font-medium",
                    )
                    .trigger(trigger)
                    .on_change(move |chosen: &Option<SharedString>| {
                        if let Some(raw) = chosen
                            && let Ok(sz) = raw.parse::<u32>()
                        {
                            font_size.set(sz);
                            send_action(&tx_size, SettingsAction::SetFontSize(sz));
                        }
                    })
                    .into_element()
            };

            let notifications_switch = kumu_switch_bare(move || notifications_enabled.get())
                .accessibility_label("Agent 通知")
                .on_change(move |checked: &bool| {
                    notifications_enabled.set(*checked);
                    send_action(&tx_notif, SettingsAction::SetNotifications(*checked));
                })
                .into_element();

            let sound_switch = kumu_switch_bare(move || notification_sound.get())
                .accessibility_label("通知提示音")
                .on_change(move |checked: &bool| {
                    notification_sound.set(*checked);
                    send_action(&tx_sound, SettingsAction::SetNotificationSound(*checked));
                })
                .into_element();

            let auto_resume_switch = kumu_switch_bare(move || auto_resume.get())
                .accessibility_label("恢复 Agent 会话")
                .on_change(move |checked: &bool| {
                    auto_resume.set(*checked);
                    send_action(&tx_resume, SettingsAction::SetAutoResume(*checked));
                })
                .into_element();

            let general_card = section_card()
                .child(settings_row(
                    "深色外观",
                    "在亮色与暗色主题之间切换。",
                    dark_switch,
                    false,
                ))
                .child(settings_row(
                    "界面字体",
                    "选择应用的界面字体。",
                    font_picker,
                    false,
                ))
                .child(settings_row(
                    "界面字体大小",
                    "界面文字与间距的基础字号。",
                    font_size_picker,
                    false,
                ))
                .child(settings_row(
                    "Agent 通知",
                    "任务完成或需要输入、确认时提醒；应用在后台时发送系统通知。",
                    notifications_switch,
                    false,
                ))
                .child(settings_row(
                    "通知提示音",
                    "提醒时播放 macOS 提示音；仅在 Agent 通知开启时生效。",
                    sound_switch,
                    false,
                ))
                .child(settings_row(
                    "恢复 Agent 会话",
                    "启动时让 Agent 窗格回到上次的对话；关闭后只恢复窗格，以新对话开始。",
                    auto_resume_switch,
                    false,
                ))
                .child(settings_row(
                    "本地数据",
                    "项目与终端布局保存在 ~/.kumu/kumu.db。",
                    pill("SQLite", PillTone::Neutral).into_element(),
                    true,
                ));

            let updates_card = section_card()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_4()
                        .child(
                            view()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(section_title("软件更新"))
                                .child(view().mt_1().child(meta_text(format!(
                                    "当前版本 {}",
                                    env!("CARGO_PKG_VERSION")
                                )))),
                        )
                        .child(
                            kumu_button(
                                move || updater.get().button_label.to_string(),
                                ButtonVariant::Default,
                            )
                            .disabled(move || updater.get().busy)
                            .on_press(move || {
                                if updater.get_untracked().is_staged {
                                    confirm_restart_open.set(true);
                                } else {
                                    send_action(&tx_upd_btn, SettingsAction::UpdatePrimaryAction);
                                }
                            }),
                        ),
                )
                .child(
                    view()
                        .mt_2()
                        .child(meta_text(move || updater.get().message)),
                )
                .child(view().mt_2().flex_row().child(
                    kumu_button("发布页面", ButtonVariant::Ghost).on_press(move || {
                        send_action(&tx_release, SettingsAction::OpenReleasePage);
                    }),
                ));

            view()
                .flex_col()
                .gap_5()
                .child(general_card)
                .child(updates_card)
        };

        let page_content = dynamic(move || match page.get() {
            SettingsPage::Providers => providers_card().into_element(),
            SettingsPage::General => general_cards().into_element(),
        });

        let right_pane = view()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(move || active_theme().background)
            .child(
                scroll().element_ref(&content_scroll).flex_1().child(
                    view().w_full().px_8().pt_4().pb_8().items_center().child(
                        view()
                            .w_full()
                            .max_w(px(640.0))
                            .flex_col()
                            .gap_5()
                            .child(section_title(move || match page.get() {
                                SettingsPage::Providers => "服务商",
                                SettingsPage::General => "通用",
                            }))
                            .child(page_content),
                    ),
                ),
            );

        // ---- Restart confirmation modal (pure NSH dialog) ----
        let tx_restart = actions_tx.clone();
        let restart_dialog = when(
            move || confirm_restart_open.get(),
            move || {
                let tx_restart = tx_restart.clone();
                dialog()
                    .role(AccessibilityRole::AlertDialog)
                    .label("重启以完成更新")
                    .on_cancel(move || confirm_restart_open.set(false))
                    .child(
                        view()
                            .w_full()
                            .h_full()
                            .items_center()
                            .justify_center()
                            .bg(move || active_theme().foreground.opacity(0.28))
                            .child(
                                view()
                                    .w(px(380.0))
                                    .p_5()
                                    .rounded(px(10.0))
                                    .bg(move || active_theme().popover)
                                    .border_1()
                                    .border_color(move || active_theme().border)
                                    .flex_col()
                                    .gap_3()
                                    .child(section_title("重启以完成更新"))
                                    .child(meta_text(
                                        "重启将结束当前所有终端进程，请确认 Agent 任务已完成或已保存。",
                                    ))
                                    .child(
                                        view()
                                            .mt_2()
                                            .flex_row()
                                            .justify_end()
                                            .gap_2()
                                            .child(
                                                kumu_button("取消", ButtonVariant::Outline)
                                                    .on_press(move || {
                                                        confirm_restart_open.set(false);
                                                    }),
                                            )
                                            .child(
                                                kumu_button("重启更新", ButtonVariant::Primary)
                                                    .on_press(move || {
                                                        confirm_restart_open.set(false);
                                                        send_action(
                                                            &tx_restart,
                                                            SettingsAction::ConfirmRestartUpdate,
                                                        );
                                                    }),
                                            ),
                                    ),
                            ),
                    )
            },
        );

        view()
            .relative()
            .flex_row()
            .flex_1()
            .w_full()
            .h_full()
            .bg(move || active_theme().background)
            .font_family(move || {
                active_theme()
                    .font_sans
                    .clone()
                    .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
            })
            .child(sidebar)
            .child(right_pane)
            .child(restart_dialog)
            .child(crate::nsh_ui::toast::render_toast_stack(
                Some(toast_rx),
                nsh_base::Anchor::TopRight,
                None,
            ))
    }
}

pub struct SettingsWindow {
    mount_id: u64,
    updater: Entity<Updater>,
    app_view: Entity<AppView>,
    events_tx: UnboundedSender<HostEvent>,
    next_toast_id: u64,
    integration_busy: Option<&'static str>,
    _theme_sub: Subscription,
    _updater_sub: Subscription,
    _actions_task: Task<()>,
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        let _ = take_mount(self.mount_id);
    }
}

impl SettingsWindow {
    pub fn new(updater: Entity<Updater>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (events_tx, events_rx) = unbounded::<HostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<SettingsAction>();

        let kumu_theme = KumuTheme::global(cx);
        let initial_nsh_theme = nsh_theme(kumu_theme);
        let initial_appearance = appearance(kumu_theme.mode);

        let system_fonts = theme::system_fonts(cx)
            .into_iter()
            .map(|(label, family)| (label.to_string(), family.to_string()))
            .collect();
        let notif = cx.global::<NotificationSettings>();
        let initial_state = InitialSettingsState {
            dark: kumu_theme.mode.is_dark(),
            font_family: kumu_theme.font_family.to_string(),
            font_size: kumu_theme.font_size.round() as u32,
            system_fonts,
            notifications_enabled: notif.enabled,
            notification_sound: notif.sound,
            auto_resume: db::get_setting(db::SETTING_AUTO_RESUME).as_deref() != Some("0"),
            updater: UpdateSnapshot::from_status(updater.read(cx).status()),
        };

        let mount_id = register_mount(SettingsMount {
            initial: initial_state,
            events_rx,
            actions_tx,
        });

        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Settings".to_owned(),
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(initial_nsh_theme), cx);
            view.set_appearance(initial_appearance, cx);
            view.load(nsh_platform_gpui::App::Rust(
                Root::new::<view::SettingsRoot>(move || view::SettingsRootProps {
                    mount_id: mount_id.into(),
                }),
            ));
            view
        });

        let theme_sub = cx.observe_global::<KumuTheme>(|this: &mut Self, cx| {
            let kumu_theme = KumuTheme::global(cx);
            let mapped = nsh_theme(kumu_theme);
            let mode = appearance(kumu_theme.mode);
            let dark = kumu_theme.mode.is_dark();
            let font_family = kumu_theme.font_family.to_string();
            let font_size = kumu_theme.font_size.round() as u32;
            this.app_view.update(cx, |view, cx| {
                view.set_theme(Some(mapped), cx);
                view.set_appearance(mode, cx);
            });
            let _ = this.events_tx.unbounded_send(HostEvent::ThemeChanged {
                dark,
                font_family,
                font_size,
            });
            cx.notify();
        });

        let updater_sub = cx.observe(&updater, |this, updater, cx| {
            let status = updater.read(cx).status();
            if let UpdateStatus::Errored(error) = status {
                eprintln!("[updates] {error}");
            }
            let _ = this.events_tx.unbounded_send(HostEvent::UpdaterStatus(
                UpdateSnapshot::from_status(status),
            ));
        });

        // Probe agent executables and hook/extension registrations off the UI thread.
        let probe_tx = events_tx.clone();
        cx.spawn(async move |_, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    AGENTS
                        .iter()
                        .map(|agent| {
                            let is_installed = crate::agent::installed_with_shell_env(agent.id);
                            let status =
                                integration::status(agent.id).map_err(|error| error.to_string());
                            (agent.id, is_installed, status)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = probe_tx.unbounded_send(HostEvent::AgentsProbed(results));
        })
        .detach();

        let actions_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = actions_rx.next().await {
                let res = this.update_in(cx, |this, window, cx| {
                    this.handle_action(action, window, cx);
                });
                if res.is_err() {
                    break;
                }
            }
        });

        Self {
            mount_id,
            updater,
            app_view,
            events_tx,
            next_toast_id: 1,
            integration_busy: None,
            _theme_sub: theme_sub,
            _updater_sub: updater_sub,
            _actions_task: actions_task,
        }
    }

    fn push_toast(&mut self, item: ToastItem) {
        let _ = self.events_tx.unbounded_send(HostEvent::Toast(item));
    }

    fn next_toast_key(&mut self) -> String {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        format!("settings-toast-{id}")
    }

    fn handle_action(
        &mut self,
        action: SettingsAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SettingsAction::CloseWindow => window.remove_window(),
            SettingsAction::SetDarkMode(dark) => {
                theme::change_mode(
                    if dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                    Some(window),
                    cx,
                );
            }
            SettingsAction::SetFontFamily(family) => {
                if KumuTheme::global(cx).font_family.as_ref() == family.as_str() {
                    return;
                }
                if let Err(error) = db::set_setting(db::SETTING_FONT, &family) {
                    let key = self.next_toast_key();
                    self.push_toast(ToastItem::error(key, format!("保存字体失败：{error}")));
                }
                theme::set_font_family(cx, &family);
            }
            SettingsAction::SetFontSize(size) => {
                if (size as f32 - KumuTheme::global(cx).font_size).abs() < 0.01 {
                    return;
                }
                if let Err(error) = db::set_setting(db::SETTING_FONT_SIZE, &size.to_string()) {
                    let key = self.next_toast_key();
                    self.push_toast(ToastItem::error(key, format!("保存字号失败：{error}")));
                }
                theme::set_font_size(cx, size as f32);
            }
            SettingsAction::SetNotifications(enabled) => {
                if let Err(error) =
                    db::set_setting(db::SETTING_NOTIFICATIONS, if enabled { "1" } else { "0" })
                {
                    let key = self.next_toast_key();
                    self.push_toast(ToastItem::error(key, format!("保存通知设置失败：{error}")));
                    let cur = cx.global::<NotificationSettings>();
                    let _ = self
                        .events_tx
                        .unbounded_send(HostEvent::NotificationSettings {
                            enabled: cur.enabled,
                            sound: cur.sound,
                        });
                    return;
                }
                let sound = cx.global::<NotificationSettings>().sound;
                cx.set_global(NotificationSettings { enabled, sound });
                window.refresh();
            }
            SettingsAction::SetNotificationSound(sound) => {
                if let Err(error) = db::set_setting(
                    db::SETTING_NOTIFICATION_SOUND,
                    if sound { "1" } else { "0" },
                ) {
                    let key = self.next_toast_key();
                    self.push_toast(ToastItem::error(
                        key,
                        format!("保存提示音设置失败：{error}"),
                    ));
                    let cur = cx.global::<NotificationSettings>();
                    let _ = self
                        .events_tx
                        .unbounded_send(HostEvent::NotificationSettings {
                            enabled: cur.enabled,
                            sound: cur.sound,
                        });
                    return;
                }
                let enabled = cx.global::<NotificationSettings>().enabled;
                cx.set_global(NotificationSettings { enabled, sound });
                window.refresh();
            }
            SettingsAction::SetAutoResume(enabled) => {
                if let Err(error) =
                    db::set_setting(db::SETTING_AUTO_RESUME, if enabled { "1" } else { "0" })
                {
                    let key = self.next_toast_key();
                    self.push_toast(ToastItem::error(
                        key,
                        format!("保存会话恢复设置失败：{error}"),
                    ));
                    let current = db::get_setting(db::SETTING_AUTO_RESUME).as_deref() != Some("0");
                    let _ = self
                        .events_tx
                        .unbounded_send(HostEvent::AutoResume(current));
                }
            }
            SettingsAction::ToggleIntegration { agent, remove } => {
                self.change_integration(agent, remove, window, cx);
            }
            SettingsAction::UpdatePrimaryAction => match self.updater.read(cx).status().clone() {
                UpdateStatus::Available(_) => {
                    self.updater
                        .update(cx, |updater, cx| updater.download_and_install(cx));
                }
                UpdateStatus::Downloading { .. } => {
                    self.updater.update(cx, |updater, cx| updater.cancel(cx));
                }
                UpdateStatus::Idle | UpdateStatus::UpToDate | UpdateStatus::Errored(_) => {
                    self.updater.update(cx, |updater, cx| updater.check(cx));
                }
                _ => {}
            },
            SettingsAction::ConfirmRestartUpdate => {
                self.updater.update(cx, |updater, cx| updater.restart(cx));
            }
            SettingsAction::OpenReleasePage => {
                cx.open_url("https://github.com/yankewei/kumu/releases/latest");
            }
        }
    }

    fn change_integration(
        &mut self,
        agent: &'static str,
        remove: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.integration_busy.is_some() {
            return;
        }
        self.integration_busy = Some(agent);
        let _ = self
            .events_tx
            .unbounded_send(HostEvent::IntegrationBusy(Some(agent)));

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
            let _ = this.update(cx, |this, _cx| {
                this.integration_busy = None;
                let _ = this
                    .events_tx
                    .unbounded_send(HostEvent::IntegrationFinished { agent, status });
                let key = this.next_toast_key();
                let toast = match result {
                    Ok(()) => ToastItem::success(
                        key,
                        if remove {
                            "已关闭通知；请重新启动 Agent。"
                        } else if agent == "codex" {
                            "已开启通知；重新启动 Codex 后，请在 /hooks 中审查并信任。"
                        } else {
                            "已开启通知；请重新启动 Agent。"
                        },
                    ),
                    Err(error) => ToastItem::error(key, format!("通知设置失败：{error}")),
                };
                this.push_toast(toast);
            });
        })
        .detach();
    }

    /// Open the update card from the main window's persistent notice.
    /// A ready-to-restart notice also opens the confirmation dialog.
    pub fn show_updates(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let prompt_restart = matches!(self.updater.read(cx).status(), UpdateStatus::Staged(_));
        let _ = self
            .events_tx
            .unbounded_send(HostEvent::ShowUpdates { prompt_restart });
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = KumuTheme::global(cx);
        let title_bar = TitleBar::new().child(div().text_sm().font_bold().child("kumu"));
        div()
            .flex()
            .flex_col()
            .size_full()
            .font_family(theme.font_family.clone())
            .bg(theme.colors.background)
            .text_color(theme.colors.foreground)
            .child(title_bar)
            .child(div().flex_1().min_h_0().child(self.app_view.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn settings_nsh_navigation_toggles_and_restart_dialog() {
        let (events_tx, events_rx) = unbounded::<HostEvent>();
        let (actions_tx, mut actions_rx) = unbounded::<SettingsAction>();
        let mount_id = register_mount(SettingsMount {
            initial: InitialSettingsState {
                dark: false,
                font_family: crate::theme::SYSTEM_FONT_FAMILY.to_string(),
                font_size: 16,
                system_fonts: vec![
                    ("系统字体".to_string(), ".SystemUIFont".to_string()),
                    ("JetBrains Mono".to_string(), "JetBrains Mono".to_string()),
                ],
                notifications_enabled: true,
                notification_sound: true,
                auto_resume: true,
                updater: UpdateSnapshot::from_status(&UpdateStatus::Idle),
            },
            events_rx,
            actions_tx,
        });

        let root =
            nsh_core::element::Root::new::<view::SettingsRoot>(move || view::SettingsRootProps {
                mount_id: mount_id.into(),
            });
        let mut app =
            nsh_testing::Headless::start(EngineConfig::new(root)).expect("start settings NSH");

        app.wait_for("编码 Agent").expect("providers page renders");

        // Switch to General page and toggle dark mode.
        app.press("通用").expect("switch to General tab");
        app.wait_for("深色外观").expect("general card renders");
        app.press("深色外观").expect("toggle dark mode");
        assert_eq!(
            actions_rx.try_recv().ok(),
            Some(SettingsAction::SetDarkMode(true))
        );

        // Push Staged update from host and verify confirmation modal opens.
        let staged = UpdateSnapshot::from_status(&UpdateStatus::Staged(
            crate::updates::Version::new(0, 2, 0),
        ));
        events_tx
            .unbounded_send(HostEvent::UpdaterStatus(staged))
            .expect("send staged status");
        app.wait_for("版本 0.2.0 已安装，重启后生效。")
            .expect("updates card reflects staged version");
        app.press("重启更新").expect("open restart confirmation");
        app.wait_for("重启以完成更新")
            .expect("restart confirmation dialog opens");
    }
}
