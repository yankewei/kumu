use std::collections::HashMap;
use std::rc::Rc;

use futures::channel::mpsc::UnboundedSender;
use nsh_base::{SelectEntry, SelectItem, SelectValue, input, select, theme as active_theme};
use nsh_core::prelude::*;

use super::{AgentStatusRow, SettingsAction, UpdateSnapshot};
use crate::agent::{AGENTS, integration};
use crate::nsh_ui::assets::{agent_logo, icon_svg};
use crate::nsh_ui::components::{
    ButtonVariant, PillTone, kumu_button, kumu_switch_bare, meta_text, pill, row_title,
    section_card, section_title,
};
use crate::nsh_ui::theme::{TOKEN_ICON_TILE, kumu_color, text_xs_size};

pub(super) fn send_action(tx: &Option<UnboundedSender<SettingsAction>>, action: SettingsAction) {
    if let Some(tx) = tx {
        let _ = tx.unbounded_send(action);
    }
}

#[derive(Clone)]
pub(super) struct ProvidersPageState {
    pub search_query: Signal<String>,
    pub agents_state: Signal<HashMap<&'static str, AgentStatusRow>>,
    pub agent_args_signals: Rc<HashMap<&'static str, Signal<String>>>,
    pub integration_busy: Signal<Option<&'static str>>,
    pub actions_tx: Option<UnboundedSender<SettingsAction>>,
}

#[derive(Clone)]
pub(super) struct GeneralPageState {
    pub dark: Signal<bool>,
    pub font_family: Signal<String>,
    pub font_size: Signal<u32>,
    pub notifications_enabled: Signal<bool>,
    pub notification_sound: Signal<bool>,
    pub auto_resume: Signal<bool>,
    pub updater: Signal<UpdateSnapshot>,
    pub confirm_restart_open: Signal<bool>,
    pub font_items: Vec<SelectItem>,
    pub font_label_map: HashMap<String, String>,
    pub font_size_items: Vec<SelectItem>,
    pub actions_tx: Option<UnboundedSender<SettingsAction>>,
}

pub(super) fn render_providers_card(state: ProvidersPageState) -> impl IntoElement {
    let ProvidersPageState {
        search_query,
        agents_state,
        agent_args_signals,
        integration_busy,
        actions_tx,
    } = state;

    section_card()
        .child(
            view()
                .flex_col()
                .gap_1()
                .child(section_title("编码 Agent"))
                .child(meta_text(
                    "开启通知会注册到 Agent 的配置或扩展目录；可为已安装的 Agent 配置默认启动参数（新建窗格生效）。",
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
                let row_el = render_agent_row(
                    a.id,
                    a.name,
                    a.desc,
                    a.logo,
                    states.get(a.id),
                    busy_agent,
                    agent_args_signals.get(a.id).copied(),
                    &actions_tx,
                    idx + 1 < total,
                );
                rows = rows.child(row_el);
            }
            rows
        }))
}

#[allow(clippy::too_many_arguments)]
fn render_agent_row(
    agent_id: &'static str,
    agent_name: &'static str,
    agent_desc: &'static str,
    logo: Option<&'static str>,
    row_state: Option<&AgentStatusRow>,
    busy_agent: Option<&'static str>,
    arg_sig: Option<Signal<String>>,
    actions_tx: &Option<UnboundedSender<SettingsAction>>,
    has_bottom_border: bool,
) -> impl IntoElement {
    let installed = row_state.is_some_and(|r| r.installed);
    let status = row_state.and_then(|r| r.status.as_ref());
    let registered = matches!(status, Some(Ok(integration::Status::Registered)));
    let needs_repair = matches!(status, Some(Ok(integration::Status::NeedsRepair)));

    let notification_pill = if installed && registered {
        Some((PillTone::Accent, "已开启"))
    } else if installed && needs_repair {
        Some((PillTone::Neutral, "需更新"))
    } else {
        None
    };
    let hint = (installed && (registered || needs_repair) && agent_id == "codex")
        .then_some("在 Codex /hooks 中确认信任");
    let (action_label, removing) = if registered {
        ("关闭通知", true)
    } else if needs_repair {
        ("更新配置", false)
    } else {
        ("开启通知", false)
    };

    let mut logo_wrap = view().child(agent_logo(logo, 16.0));
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
        text(agent_name)
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
        .child(view().mt_0p5().child(meta_text(agent_desc)));
    if let Some(hint_text) = hint {
        info_col = info_col.child(view().mt_0p5().child(meta_text(hint_text)));
    }

    let mut top_row = view()
        .flex_row()
        .items_center()
        .gap_3()
        .child(icon_tile)
        .child(info_col);

    if installed {
        let is_this_busy = busy_agent == Some(agent_id);
        let any_busy = busy_agent.is_some();
        let tx = actions_tx.clone();
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
        top_row = top_row.child(kumu_button(btn_text, variant).disabled(any_busy).on_press(
            move || {
                send_action(
                    &tx,
                    SettingsAction::ToggleIntegration {
                        agent: agent_id,
                        remove: removing,
                    },
                );
            },
        ));
    }

    let mut row_el = view().flex_col().py_3().gap_2().child(top_row);
    if installed && let Some(arg_sig) = arg_sig {
        let tx_args = actions_tx.clone();
        row_el = row_el.child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .pl(px(42.0))
                .child(meta_text("启动参数").flex_shrink_0())
                .child(
                    input()
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .rounded(move || active_theme().radius)
                        .border_1()
                        .border_color(move || active_theme().input)
                        .bg(move || active_theme().background)
                        .text_size(text_xs_size)
                        .text_color(move || active_theme().foreground)
                        .placeholder("可选，如 --model ...（新建窗格生效）")
                        .placeholder_color(move || active_theme().muted_foreground.light)
                        .value(move || arg_sig.get())
                        .on_change(move |val: &str| {
                            arg_sig.set(val.to_owned());
                            send_action(
                                &tx_args,
                                SettingsAction::SetAgentArgs {
                                    agent: agent_id,
                                    args: val.to_owned(),
                                },
                            );
                        }),
                ),
        );
    }

    if has_bottom_border {
        row_el = row_el
            .border_b_1()
            .border_b_color(move || active_theme().border);
    }
    row_el
}

fn settings_row(
    title: &'static str,
    desc: &'static str,
    control: AnyElement,
    last: bool,
) -> impl IntoElement {
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
}

fn render_font_picker(
    font_family: Signal<String>,
    font_items: Vec<SelectItem>,
    labels: HashMap<String, String>,
    tx_font: Option<UnboundedSender<SettingsAction>>,
) -> AnyElement {
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
        .popup_class("bg-popover border border-border rounded-lg shadow-md overflow-hidden p-1")
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
}

fn render_font_size_picker(
    font_size: Signal<u32>,
    font_size_items: Vec<SelectItem>,
    tx_size: Option<UnboundedSender<SettingsAction>>,
) -> AnyElement {
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
        .popup_class("bg-popover border border-border rounded-lg shadow-md overflow-hidden p-1")
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
}

fn render_general_settings_card(state: &GeneralPageState) -> impl IntoElement {
    let dark = state.dark;
    let notifications_enabled = state.notifications_enabled;
    let notification_sound = state.notification_sound;
    let auto_resume = state.auto_resume;

    let tx_dark = state.actions_tx.clone();
    let tx_font = state.actions_tx.clone();
    let tx_size = state.actions_tx.clone();
    let tx_notif = state.actions_tx.clone();
    let tx_sound = state.actions_tx.clone();
    let tx_resume = state.actions_tx.clone();

    let dark_switch = kumu_switch_bare(move || dark.get())
        .accessibility_label("深色外观")
        .on_change(move |checked: &bool| {
            dark.set(*checked);
            send_action(&tx_dark, SettingsAction::SetDarkMode(*checked));
        })
        .into_element();

    let font_picker = render_font_picker(
        state.font_family,
        state.font_items.clone(),
        state.font_label_map.clone(),
        tx_font,
    );

    let font_size_picker =
        render_font_size_picker(state.font_size, state.font_size_items.clone(), tx_size);

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

    section_card()
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
        ))
}

fn render_updates_card(
    updater: Signal<UpdateSnapshot>,
    confirm_restart_open: Signal<bool>,
    actions_tx: &Option<UnboundedSender<SettingsAction>>,
) -> impl IntoElement {
    let tx_upd_btn = actions_tx.clone();
    let tx_release = actions_tx.clone();

    section_card()
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
                        .child(
                            view().mt_1().child(meta_text(format!(
                                "当前版本 {}",
                                env!("CARGO_PKG_VERSION")
                            ))),
                        ),
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
        ))
}

pub(super) fn render_general_cards(state: &GeneralPageState) -> impl IntoElement {
    let general_card = render_general_settings_card(state);
    let updates_card =
        render_updates_card(state.updater, state.confirm_restart_open, &state.actions_tx);

    view()
        .flex_col()
        .gap_5()
        .child(general_card)
        .child(updates_card)
}
