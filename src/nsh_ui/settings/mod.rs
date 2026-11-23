//! Settings window rendered in Pure Rust NSH (`nsh-platform-gpui`),
//! Waku-style: sidebar (back row + search + icon nav) on the left; a centered
//! scrollable column with card-grouped rows on the right.

mod pages;
mod view;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Task,
    Window, div,
};
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::mount::MountRegistry;
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
    agent_args: std::collections::HashMap<&'static str, String>,
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
    SetAgentArgs { agent: &'static str, args: String },
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

static MOUNTS: MountRegistry<SettingsMount> = MountRegistry::new();

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
        let _ = MOUNTS.take(self.mount_id);
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
            agent_args: AGENTS
                .iter()
                .map(|a| (a.id, db::get_agent_args(a.id)))
                .collect(),
            updater: UpdateSnapshot::from_status(updater.read(cx).status()),
        };

        let mount_id = MOUNTS.register(SettingsMount {
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
            SettingsAction::SetAgentArgs { agent, args } => {
                if let Err(error) = db::set_agent_args(agent, &args) {
                    let key = self.next_toast_key();
                    self.push_toast(ToastItem::error(
                        key,
                        format!("保存 {agent} 启动参数失败：{error}"),
                    ));
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
        let title_bar = crate::ui::titlebar::title_bar("settings-titlebar", theme, None);
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
        let mount_id = MOUNTS.register(SettingsMount {
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
                agent_args: std::collections::HashMap::new(),
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
