use std::collections::HashMap;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use nsh_base::{SelectItem, dialog, input, theme as active_theme};
use nsh_core::element::{AccessibilityRole, scroll};
use nsh_core::prelude::*;

use super::pages::{
    GeneralPageState, ProvidersPageState, render_general_cards, render_providers_card, send_action,
};
use super::{
    AgentStatusRow, HostEvent, InitialSettingsState, MOUNTS, SettingsAction, SettingsPage,
    UpdateSnapshot,
};
use crate::agent::AGENTS;
use crate::control::cli_install::CliInstallStatus;
use crate::nsh_ui::assets::icon_svg;
use crate::nsh_ui::components::{ButtonVariant, kumu_button, meta_text, section_title};
use crate::nsh_ui::theme::text_xs_size;

#[derive(Clone, Debug, PartialEq, Props)]
pub struct SettingsRootProps {
    pub mount_id: Prop<u64>,
}

fn render_sidebar(
    page: Signal<SettingsPage>,
    search_query: Signal<String>,
    on_select_page: impl Fn(SettingsPage) + Copy + 'static,
    actions_tx: Option<UnboundedSender<SettingsAction>>,
) -> impl IntoElement {
    let back_row = view()
        .role(AccessibilityRole::Button)
        .h(px(36.0))
        .px_2p5()
        .rounded(px(8.0))
        .flex_row()
        .items_center()
        .gap_2p5()
        .hover(|s| s.bg(move || active_theme().sidebar_accent.opacity(0.5)))
        .on_press(move || send_action(&actions_tx, SettingsAction::CloseWindow))
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
            icon_svg("icons/search.svg", 13.0).text_color(move || active_theme().muted_foreground),
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
                    on_select_page(target);
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

    view()
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
        .child(nav_section)
}

fn render_restart_dialog(
    confirm_restart_open: Signal<bool>,
    actions_tx: Option<UnboundedSender<SettingsAction>>,
) -> impl IntoElement {
    when(
        move || confirm_restart_open.get(),
        move || {
            let tx_restart = actions_tx.clone();
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
                                            kumu_button("取消", ButtonVariant::Outline).on_press(
                                                move || {
                                                    confirm_restart_open.set(false);
                                                },
                                            ),
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
    )
}

#[component]
pub fn SettingsRoot(props: SettingsRootProps) -> impl IntoElement {
    let mount = MOUNTS.take(props.mount_id.get_untracked());

    let (initial, events_rx, actions_tx) = match mount {
        Some(m) => (m.initial, Some(m.events_rx), Some(m.actions_tx)),
        None => (
            InitialSettingsState {
                dark: false,
                terminal_theme: crate::theme::TerminalTheme::FollowApp,
                terminal_swatches: super::build_terminal_swatches(
                    &crate::theme::KumuTheme::load_bundled(),
                ),
                font_family: crate::theme::SYSTEM_FONT_FAMILY.to_string(),
                font_size: crate::theme::DEFAULT_FONT_SIZE as u32,
                system_fonts: vec![(
                    crate::theme::SYSTEM_FONT_LABEL.to_string(),
                    crate::theme::SYSTEM_FONT_FAMILY.to_string(),
                )],
                term_font_family: crate::fonts::TERM_FONT.to_string(),
                term_font_size: crate::theme::DEFAULT_TERM_FONT_SIZE as u32,
                terminal_fonts: vec![(
                    format!("{}（内置）", crate::fonts::TERM_FONT),
                    crate::fonts::TERM_FONT.to_string(),
                )],
                notifications_enabled: true,
                notification_sound: true,
                auto_resume: true,
                agent_args: HashMap::new(),
                updater: UpdateSnapshot::from_status(&crate::updates::UpdateStatus::Idle),
            },
            None,
            None,
        ),
    };

    let page = signal(SettingsPage::Providers);
    let search_query = signal(String::new());
    let dark = signal(initial.dark);
    let terminal_theme = signal(initial.terminal_theme);
    let font_family = signal(initial.font_family);
    let font_size = signal(initial.font_size);
    let term_font_family = signal(initial.term_font_family);
    let term_font_size = signal(initial.term_font_size);
    let notifications_enabled = signal(initial.notifications_enabled);
    let notification_sound = signal(initial.notification_sound);
    let auto_resume = signal(initial.auto_resume);
    let updater = signal(initial.updater);
    let agents_state = signal(HashMap::<&'static str, AgentStatusRow>::new());
    let agent_args_signals: Rc<HashMap<&'static str, Signal<String>>> = Rc::new(
        AGENTS
            .iter()
            .map(|a| {
                let initial_val = initial.agent_args.get(a.id).cloned().unwrap_or_default();
                (a.id, signal(initial_val))
            })
            .collect(),
    );
    let integration_busy = signal(Option::<&'static str>::None);
    let cli_status = signal(Option::<CliInstallStatus>::None);
    let cli_busy = signal(false);
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
    let term_font_items: Vec<SelectItem> = initial
        .terminal_fonts
        .into_iter()
        .map(|(label, family)| SelectItem::new(family, label))
        .collect();
    let term_font_label_map: HashMap<String, String> = term_font_items
        .iter()
        .map(|item| (item.value.to_string(), item.title.to_string()))
        .collect();
    let term_font_size_items: Vec<SelectItem> = crate::theme::TERM_FONT_SIZES
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
                        term_font_family: tfam,
                        term_font_size: tsz,
                    } => {
                        dark.set(is_dark);
                        font_family.set(fam);
                        font_size.set(sz);
                        term_font_family.set(tfam);
                        term_font_size.set(tsz);
                    }
                    HostEvent::TerminalThemeChanged(term_theme) => {
                        terminal_theme.set(term_theme);
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
                    HostEvent::CliStatus(status) => {
                        cli_status.set(Some(status));
                    }
                    HostEvent::CliBusy(busy) => {
                        cli_busy.set(busy);
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
                        let _ =
                            toast_tx.unbounded_send(crate::nsh_ui::toast::ToastCommand::Push(item));
                    }
                }
            }
        });
    }

    let sidebar = render_sidebar(
        page,
        search_query,
        move |target| {
            page.set(target);
            content_scroll.scroll_to_offset(px(0.0), false);
        },
        actions_tx.clone(),
    );

    let providers_state = ProvidersPageState {
        search_query,
        agents_state,
        agent_args_signals,
        integration_busy,
        actions_tx: actions_tx.clone(),
    };
    let general_state = GeneralPageState {
        dark,
        terminal_theme,
        terminal_swatches: Rc::new(initial.terminal_swatches),
        font_family,
        font_size,
        term_font_family,
        term_font_size,
        notifications_enabled,
        notification_sound,
        auto_resume,
        cli_status,
        cli_busy,
        updater,
        confirm_restart_open,
        font_items,
        font_label_map,
        font_size_items,
        term_font_items,
        term_font_label_map,
        term_font_size_items,
        actions_tx: actions_tx.clone(),
    };

    let page_content = dynamic(move || match page.get() {
        SettingsPage::Providers => render_providers_card(providers_state.clone()).into_element(),
        SettingsPage::General => render_general_cards(&general_state).into_element(),
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

    let restart_dialog = render_restart_dialog(confirm_restart_open, actions_tx);

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
