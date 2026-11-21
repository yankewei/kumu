//! Pure Rust NSH foreground toast stack built over `nsh_base::toast_stack` +
//! `nsh_base::ToastManager`.
//!
//! Can be mounted directly inside any NSH root component (e.g. Settings window
//! or the unified Home NSH root), and also provides [`NshToastHost`] for GPUI
//! windows hosting foreground notifications with click-to-action and dismissal.

#[cfg(test)]
use std::collections::HashMap;
use std::rc::Rc;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt;
#[cfg(test)]
use futures::channel::mpsc::unbounded;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use nsh_base::{
    Anchor, ToastManager, ToastMotion, ToastOptions, ToastStackState, ToastTransitionStatus,
    theme as active_theme, toast, toast_stack,
};
use nsh_core::element::AccessibilityRole;
use nsh_core::prelude::*;
use nsh_core::time::now;

use super::assets::icon_svg;
use super::theme::{TOKEN_LINK, TOKEN_SUCCESS, TOKEN_WARNING, kumu_color, text_xs_size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ToastTone {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastItem {
    pub id: String,
    pub title: Option<String>,
    pub message: String,
    pub tone: ToastTone,
    pub clickable: bool,
}

impl ToastItem {
    pub fn info(id: impl Into<String>, title: Option<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title,
            message: message.into(),
            tone: ToastTone::Info,
            clickable: false,
        }
    }

    pub fn success(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: None,
            message: message.into(),
            tone: ToastTone::Success,
            clickable: false,
        }
    }

    pub fn error(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: None,
            message: message.into(),
            tone: ToastTone::Error,
            clickable: false,
        }
    }

    pub fn clickable(mut self, clickable: bool) -> Self {
        self.clickable = clickable;
        self
    }

    fn timeout(&self) -> Duration {
        match self.tone {
            ToastTone::Error => Duration::from_secs(7),
            ToastTone::Warning => Duration::from_secs(6),
            ToastTone::Info | ToastTone::Success => Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ToastCommand {
    Push(ToastItem),
    Dismiss(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToastHostEvent {
    Clicked(String),
    CountChanged(usize),
}

fn tone_color(tone: ToastTone) -> AdaptiveColor {
    match tone {
        ToastTone::Info => kumu_color(TOKEN_LINK, |t| t.primary),
        ToastTone::Success => kumu_color(TOKEN_SUCCESS, |t| t.primary),
        ToastTone::Warning => kumu_color(TOKEN_WARNING, |t| t.destructive),
        ToastTone::Error => active_theme().destructive,
    }
}

fn tone_icon(tone: ToastTone) -> &'static str {
    match tone {
        ToastTone::Info => "icons/info.svg",
        ToastTone::Success => "icons/check.svg",
        ToastTone::Warning | ToastTone::Error => "icons/triangle-alert.svg",
    }
}

/// Mount an NSH `toast_stack` inside any NSH component, fed by `cmd_rx`.
pub fn render_toast_stack(
    cmd_rx: Option<UnboundedReceiver<ToastCommand>>,
    placement: Anchor,
    on_event: Option<UnboundedSender<ToastHostEvent>>,
) -> impl IntoElement {
    let manager = stored(ToastManager::<String, ToastItem>::new(ToastMotion::sonner()));
    let shown = signal(Vec::<(String, ToastItem, ToastTransitionStatus)>::new());
    let state = ToastStackState::new();

    let on_event_sync = on_event.clone();
    let sync: Rc<dyn Fn()> = Rc::new(move || {
        let next: Vec<(String, ToastItem, ToastTransitionStatus)> = manager.with(|m| {
            m.visible(3)
                .map(|(id, item, status)| (id.clone(), item.clone(), status))
                .collect()
        });
        let total = manager.with(|m| m.len());
        shown.set(next);
        if let Some(tx) = &on_event_sync {
            let _ = tx.unbounded_send(ToastHostEvent::CountChanged(total));
        }
    });

    let sync_tick = sync.clone();
    set_interval(Duration::from_millis(80), move || {
        if manager.with(|m| m.advance(now(), state.is_expanded()).changed) {
            sync_tick();
        }
    });

    if let Some(mut rx) = cmd_rx {
        let sync_cmd = sync.clone();
        spawn(async move {
            while let Some(cmd) = rx.next().await {
                let ts = now();
                match cmd {
                    ToastCommand::Push(item) => {
                        let id = item.id.clone();
                        let opts = ToastOptions::timeout(Some(item.timeout()));
                        manager.with(|m| {
                            m.dismiss(&id, ts);
                            m.push(id, item, opts, ts);
                        });
                    }
                    ToastCommand::Dismiss(id) => {
                        manager.with(|m| {
                            m.dismiss(&id, ts);
                        });
                    }
                }
                sync_cmd();
            }
        });
    }

    view().absolute().top_3().right_3().child(
        toast_stack()
            .placement(placement)
            .state(state)
            .w(px(336.0))
            .child(each(
                shown,
                |(id, _, _)| id.clone(),
                move |entry, _| {
                    let on_click_tx = on_event.clone();
                    let sync_close = sync.clone();
                    let sync_click = sync.clone();
                    let sync_dismiss = sync.clone();
                    let current = entry.get_untracked().1;
                    let id_click = current.id.clone();
                    let id_close = current.id.clone();
                    let tone = current.tone;
                    let clickable = current.clickable;
                    let title = current.title.clone();
                    let message = current.message.clone();
                    let a11y_label = match &title {
                        Some(t) => format!("{t}: {message}"),
                        None => message.clone(),
                    };

                    let mut body = view().flex_col().flex_1().min_w_0().gap_0p5();
                    if let Some(title) = title {
                        body = body.child(
                            text(title)
                                .text_size(text_xs_size)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || active_theme().popover_foreground),
                        );
                    }
                    body = body.child(
                        text(message)
                            .text_size(text_xs_size)
                            .text_color(move || active_theme().muted_foreground),
                    );

                    let mut card = view()
                        .flex_row()
                        .w_full()
                        .items_start()
                        .gap_2()
                        .px_3()
                        .py_2p5()
                        .rounded(move || active_theme().radius)
                        .bg(move || active_theme().popover)
                        .border_1()
                        .border_color(move || active_theme().border)
                        .shadow_lg()
                        .font_family(move || {
                            active_theme().font_sans.clone().unwrap_or_else(|| {
                                SharedString::from(crate::theme::SYSTEM_FONT_FAMILY)
                            })
                        })
                        .child(view().pt_0p5().flex_shrink_0().child(
                            icon_svg(tone_icon(tone), 14.0).text_color(move || tone_color(tone)),
                        ))
                        .child(body)
                        .child(
                            view()
                                .role(AccessibilityRole::Button)
                                .label("关闭通知")
                                .flex_shrink_0()
                                .p_0p5()
                                .rounded(move || active_theme().radius)
                                .hover(|s| s.bg(move || active_theme().accent.opacity(0.6)))
                                .on_press(move || {
                                    manager.with(|m| {
                                        m.dismiss(&id_close, now());
                                    });
                                    sync_close();
                                })
                                .child(
                                    icon_svg("icons/x.svg", 12.0)
                                        .text_color(move || active_theme().muted_foreground),
                                ),
                        );

                    if clickable {
                        card = card
                            .role(AccessibilityRole::Button)
                            .hover(|s| s.bg(move || active_theme().accent.opacity(0.35)))
                            .on_press(move || {
                                if let Some(tx) = &on_click_tx {
                                    let _ = tx
                                        .unbounded_send(ToastHostEvent::Clicked(id_click.clone()));
                                }
                                manager.with(|m| {
                                    m.dismiss(&id_click, now());
                                });
                                sync_click();
                            });
                    }

                    toast()
                        .transition_status(move || entry.get().2)
                        .accessibility_label(Some(SharedString::from(a11y_label)))
                        .on_dismiss_request(move || {
                            let id = entry.get_untracked().0;
                            manager.with(|m| {
                                m.dismiss(&id, now());
                            });
                            sync_dismiss();
                        })
                        .transition_all()
                        .duration(180.0)
                        .starting_class("opacity-0 translate-y-2")
                        .ending_class("opacity-0 scale-95")
                        .child(card)
                },
            )),
    )
}

#[cfg(test)]
struct ToastOverlayMount {
    cmd_rx: UnboundedReceiver<ToastCommand>,
    event_tx: UnboundedSender<ToastHostEvent>,
}

#[cfg(test)]
static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static PENDING_MOUNTS: OnceLock<Mutex<HashMap<u64, ToastOverlayMount>>> = OnceLock::new();

#[cfg(test)]
fn pending_mounts() -> &'static Mutex<HashMap<u64, ToastOverlayMount>> {
    PENDING_MOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn register_mount(mount: ToastOverlayMount) -> u64 {
    let id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = pending_mounts().lock() {
        map.insert(id, mount);
    }
    id
}

#[cfg(test)]
fn take_mount(id: u64) -> Option<ToastOverlayMount> {
    pending_mounts().lock().ok()?.remove(&id)
}

#[cfg(test)]
mod overlay_view {
    use nsh_base::Anchor;
    use nsh_core::prelude::*;

    use super::{render_toast_stack, take_mount};

    #[derive(Clone, Debug, PartialEq, Props)]
    pub(super) struct ToastOverlayRootProps {
        pub(super) mount_id: Prop<u64>,
    }

    #[component]
    pub(super) fn ToastOverlayRoot(props: ToastOverlayRootProps) -> impl IntoElement {
        let mount = take_mount(props.mount_id.get_untracked());
        let (cmd_rx, event_tx) = match mount {
            Some(m) => (Some(m.cmd_rx), Some(m.event_tx)),
            None => (None, None),
        };
        view()
            .relative()
            .w_full()
            .h_full()
            .child(render_toast_stack(cmd_rx, Anchor::TopRight, event_tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_engine::EngineConfig;

    #[test]
    fn toast_stack_renders_clicks_and_dismisses_headlessly() {
        let (cmd_tx, cmd_rx) = unbounded::<ToastCommand>();
        let (event_tx, mut event_rx) = unbounded::<ToastHostEvent>();
        let mount_id = register_mount(ToastOverlayMount { cmd_rx, event_tx });
        let root = nsh_core::element::Root::new::<overlay_view::ToastOverlayRoot>(move || {
            overlay_view::ToastOverlayRootProps {
                mount_id: mount_id.into(),
            }
        });
        let mut app = nsh_testing::Headless::start(EngineConfig::new(root))
            .expect("start headless toast stack");

        cmd_tx
            .unbounded_send(ToastCommand::Push(
                ToastItem::info("pane-7", Some("kumu · codex".into()), "任务已完成")
                    .clickable(true),
            ))
            .expect("push toast");
        app.wait_for("任务已完成").expect("toast message renders");
        app.press("任务已完成").expect("click toast");

        let mut clicked = false;
        while let Ok(ev) = event_rx.try_recv() {
            if ev == ToastHostEvent::Clicked("pane-7".into()) {
                clicked = true;
            }
        }
        assert!(clicked, "clicking toast must emit Clicked(pane-7)");
    }
}
