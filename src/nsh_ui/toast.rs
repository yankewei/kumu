//! Pure Rust NSH foreground toast stack built over `nsh_base::toast_stack` +
//! `nsh_base::ToastManager`.
//!
//! - Settings window mounts `render_toast_stack` directly inside its NSH root
//!   (no native GPUI views underneath).
//! - Home window mounts [`ToastOverlay`] as a GPUI-level overlay above
//!   `NshHomeShell` so `native("kumu.terminal")` hitboxes never intercept hover
//!   or clicks over notification cards.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{AppContext, Context, Entity, Task, Window};
use nsh_base::{
    Anchor, ToastManager, ToastMotion, ToastOptions, ToastTransitionStatus, theme as active_theme,
    toast, toast_stack,
};
use nsh_core::element::AccessibilityRole;
use nsh_core::prelude::*;
use nsh_core::time::now;
use nsh_platform_gpui::{AppOptions, AppView, Root};

use super::assets::icon_svg;
use super::mount::MountRegistry;
use super::theme::{TOKEN_LINK, TOKEN_SUCCESS, appearance, kumu_color, nsh_theme, text_xs_size};
use crate::theme::KumuTheme;

const TOAST_STACK_WIDTH: f32 = 336.0;
const TOAST_OVERLAY_PAD: f32 = 12.0;
const TOAST_INITIAL_HEIGHT: f32 = 80.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastTone {
    Info,
    Success,
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
            ToastTone::Info | ToastTone::Success => Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ToastCommand {
    Push(ToastItem),
    Dismiss(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToastHostEvent {
    Clicked(String),
    Visibility(bool),
    LayoutHeight(f32),
}

fn tone_color(tone: ToastTone) -> AdaptiveColor {
    match tone {
        ToastTone::Info => kumu_color(TOKEN_LINK, |t| t.primary),
        ToastTone::Success => kumu_color(TOKEN_SUCCESS, |t| t.primary),
        ToastTone::Error => active_theme().destructive,
    }
}

fn tone_icon(tone: ToastTone) -> &'static str {
    match tone {
        ToastTone::Info => "icons/info.svg",
        ToastTone::Success => "icons/check.svg",
        ToastTone::Error => "icons/triangle-alert.svg",
    }
}

/// Mount an NSH `toast_stack` inside any NSH component, fed by `cmd_rx`.
pub fn render_toast_stack(
    cmd_rx: Option<UnboundedReceiver<ToastCommand>>,
    placement: Anchor,
    on_event: Option<UnboundedSender<ToastHostEvent>>,
) -> impl IntoElement {
    view()
        .absolute()
        .top_3()
        .right_3()
        .child(render_toast_stack_content(cmd_rx, placement, on_event))
}

fn render_toast_stack_content(
    cmd_rx: Option<UnboundedReceiver<ToastCommand>>,
    placement: Anchor,
    on_event: Option<UnboundedSender<ToastHostEvent>>,
) -> impl IntoElement {
    let manager = stored(ToastManager::<String, ToastItem>::new(ToastMotion::sonner()));
    let shown = signal(Vec::<(String, ToastItem, ToastTransitionStatus)>::new());
    let expanded = signal(false);
    let was_visible = stored(false);
    let last_height = stored(0.0_f32);

    let on_sync_event = on_event.clone();
    let sync: Rc<dyn Fn()> = Rc::new(move || {
        let next: Vec<(String, ToastItem, ToastTransitionStatus)> = manager.with(|m| {
            m.visible(3)
                .map(|(id, item, status)| (id.clone(), item.clone(), status))
                .collect()
        });
        let visible = !next.is_empty();
        shown.set(next);
        if was_visible.replace(visible) != visible {
            if !visible {
                last_height.replace(0.0);
                // Reset pause state when the stack empties so unmounting the
                // overlay while hovered never stalls future toast timeouts.
                expanded.set(false);
            }
            if let Some(tx) = &on_sync_event {
                let _ = tx.unbounded_send(ToastHostEvent::Visibility(visible));
            }
        }
    });

    type TickCallback = Rc<dyn Fn()>;
    let ticking = stored(false);
    let schedule_tick: Rc<RefCell<Option<TickCallback>>> = Rc::new(RefCell::new(None));
    {
        let sync_tick = sync.clone();
        let schedule_slot = schedule_tick.clone();
        let tick_fn: TickCallback = Rc::new(move || {
            let sync_inner = sync_tick.clone();
            let slot_inner = schedule_slot.clone();
            drop(set_timeout(Duration::from_millis(80), move || {
                if manager.with(|m| m.advance(now(), expanded.get_untracked()).changed) {
                    sync_inner();
                }
                if manager.with(|m| !m.is_empty()) {
                    if let Some(next_tick) = slot_inner.borrow().as_ref().cloned() {
                        next_tick();
                    }
                } else {
                    ticking.with(|t| *t = false);
                }
            }));
        });
        *schedule_tick.borrow_mut() = Some(tick_fn);
    }

    let ensure_ticking: Rc<dyn Fn()> = {
        let schedule_slot = schedule_tick.clone();
        Rc::new(move || {
            let should_start = manager.with(|m| !m.is_empty())
                && ticking.with(|t| {
                    if *t {
                        false
                    } else {
                        *t = true;
                        true
                    }
                });
            if should_start && let Some(tick) = schedule_slot.borrow().as_ref().cloned() {
                tick();
            }
        })
    };

    if let Some(mut rx) = cmd_rx {
        let sync_cmd = sync.clone();
        let ensure_cmd = ensure_ticking.clone();
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
                ensure_cmd();
            }
        });
    }

    let on_layout_event = on_event.clone();
    toast_stack()
        .placement(placement)
        .on_expanded_change(move |is_expanded: &bool| expanded.set(*is_expanded))
        .w(px(TOAST_STACK_WIDTH))
        .on_layout(move |frame: &Bounds<Points>| {
            let height = frame.size.height.0;
            if height > 0.0 {
                let prev = last_height.with(|h| *h);
                if (height - prev).abs() > 0.5 {
                    last_height.replace(height);
                    if let Some(tx) = &on_layout_event {
                        let _ = tx.unbounded_send(ToastHostEvent::LayoutHeight(height));
                    }
                }
            }
        })
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
                        active_theme()
                            .font_sans
                            .clone()
                            .unwrap_or_else(|| SharedString::from(crate::theme::SYSTEM_FONT_FAMILY))
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
                            .app_class("cursor-pointer")
                            .hover(|s| s.bg(move || active_theme().foreground.opacity(0.1)))
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
                        .app_class("cursor-pointer")
                        .hover(|s| s.bg(move || active_theme().accent))
                        .on_press(move || {
                            if let Some(tx) = &on_click_tx {
                                let _ =
                                    tx.unbounded_send(ToastHostEvent::Clicked(id_click.clone()));
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
        ))
}

struct ToastOverlayMount {
    cmd_rx: UnboundedReceiver<ToastCommand>,
    event_tx: UnboundedSender<ToastHostEvent>,
    placement: Anchor,
}

static OVERLAY_MOUNTS: MountRegistry<ToastOverlayMount> = MountRegistry::new();

#[derive(Clone, Debug, PartialEq, Props)]
struct ToastOverlayRootProps {
    mount_id: Prop<u64>,
}

#[component]
fn ToastOverlayRoot(props: ToastOverlayRootProps) -> impl IntoElement {
    let mount = OVERLAY_MOUNTS.take(props.mount_id.get_untracked());
    let (cmd_rx, event_tx, placement) = match mount {
        Some(m) => (Some(m.cmd_rx), Some(m.event_tx), m.placement),
        None => (None, None, Anchor::TopRight),
    };
    view()
        .w_full()
        .p(px(TOAST_OVERLAY_PAD))
        .child(render_toast_stack_content(cmd_rx, placement, event_tx))
}

/// GPUI-level foreground toast overlay owned by `KumuApp`.
///
/// Rendered above `NshHomeShell` so `native("kumu.terminal")` hitboxes never
/// intercept hover or press events over the lower half of a toast card. Sized
/// tightly to the visible toast stack (and unmounted when empty) so the rest of
/// the workspace stays interactive.
pub struct ToastOverlay {
    mount_id: u64,
    pub app_view: Entity<AppView>,
    cmd_tx: UnboundedSender<ToastCommand>,
    visible: Rc<Cell<bool>>,
    height: Rc<Cell<f32>>,
    _task: Task<()>,
}

impl Drop for ToastOverlay {
    fn drop(&mut self) {
        let _ = OVERLAY_MOUNTS.take(self.mount_id);
    }
}

impl ToastOverlay {
    pub fn new<V: 'static>(
        placement: Anchor,
        theme: &KumuTheme,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_click: impl FnMut(&mut V, String, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<ToastCommand>();
        let (event_tx, mut event_rx) = unbounded::<ToastHostEvent>();
        let mount_id = OVERLAY_MOUNTS.register(ToastOverlayMount {
            cmd_rx,
            event_tx,
            placement,
        });

        let mapped = nsh_theme(theme);
        let mode = appearance(theme.mode);
        let app_view = cx.new(|cx| {
            let options = AppOptions {
                title: "Kumu Toasts".to_owned(),
                transparent: true,
                ..Default::default()
            };
            let view = AppView::new(options, window, cx);
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
            view.load(nsh_platform_gpui::App::Rust(Root::new::<ToastOverlayRoot>(
                move || ToastOverlayRootProps {
                    mount_id: mount_id.into(),
                },
            )));
            view
        });

        let visible = Rc::new(Cell::new(false));
        let height = Rc::new(Cell::new(0.0_f32));
        let visible_task = visible.clone();
        let height_task = height.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = event_rx.next().await {
                match ev {
                    ToastHostEvent::Clicked(id) => {
                        let _ = this.update_in(cx, |host, window, cx| {
                            on_click(host, id, window, cx);
                        });
                    }
                    ToastHostEvent::Visibility(is_visible) => {
                        visible_task.set(is_visible);
                        if is_visible && height_task.get() <= 0.0 {
                            height_task.set(TOAST_INITIAL_HEIGHT);
                        } else if !is_visible {
                            height_task.set(0.0);
                        }
                        let _ = this.update_in(cx, |_host, _window, cx| {
                            cx.notify();
                        });
                    }
                    ToastHostEvent::LayoutHeight(next_height) => {
                        if visible_task.get()
                            && next_height > 0.0
                            && (height_task.get() - next_height).abs() > 0.5
                        {
                            height_task.set(next_height);
                            let _ = this.update_in(cx, |_host, _window, cx| {
                                cx.notify();
                            });
                        }
                    }
                }
            }
        });

        Self {
            mount_id,
            app_view,
            cmd_tx,
            visible,
            height,
            _task: task,
        }
    }

    pub fn push(&self, item: ToastItem) {
        let _ = self.cmd_tx.unbounded_send(ToastCommand::Push(item));
    }

    pub fn dismiss(&self, id: String) {
        let _ = self.cmd_tx.unbounded_send(ToastCommand::Dismiss(id));
    }

    pub fn sync_theme(&self, theme: &KumuTheme, cx: &mut gpui::App) {
        let (mapped, mode) = (nsh_theme(theme), appearance(theme.mode));
        self.app_view.update(cx, |view, cx| {
            view.set_theme(Some(mapped), cx);
            view.set_appearance(mode, cx);
        });
    }

    /// Overlay `(width, height)` in pixels while at least one toast is active.
    pub fn overlay_size(&self) -> Option<(f32, f32)> {
        compute_overlay_size(self.visible.get(), self.height.get())
    }
}

fn compute_overlay_size(visible: bool, measured_height: f32) -> Option<(f32, f32)> {
    if !visible {
        return None;
    }
    let h = measured_height.max(1.0);
    Some((
        TOAST_STACK_WIDTH + TOAST_OVERLAY_PAD * 2.0,
        h + TOAST_OVERLAY_PAD * 2.0,
    ))
}

#[cfg(test)]
mod tests {
    use nsh_engine::EngineConfig;

    use super::*;

    #[test]
    fn toast_overlay_emits_visibility_clicks_and_dismisses_headlessly() {
        let (cmd_tx, cmd_rx) = unbounded::<ToastCommand>();
        let (event_tx, mut event_rx) = unbounded::<ToastHostEvent>();
        let mount_id = OVERLAY_MOUNTS.register(ToastOverlayMount {
            cmd_rx,
            event_tx,
            placement: Anchor::TopRight,
        });
        let root =
            nsh_core::element::Root::new::<ToastOverlayRoot>(move || ToastOverlayRootProps {
                mount_id: mount_id.into(),
            });
        let mut app = nsh_testing::Headless::start(EngineConfig::new(root))
            .expect("start headless toast overlay");

        cmd_tx
            .unbounded_send(ToastCommand::Push(
                ToastItem::info("pane-7", Some("kumu · codex".into()), "任务已完成")
                    .clickable(true),
            ))
            .expect("push toast");
        app.wait_for("任务已完成").expect("toast message renders");
        app.press("任务已完成").expect("click toast");

        let mut saw_visible = false;
        let mut clicked = false;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                ToastHostEvent::Visibility(true) => saw_visible = true,
                ToastHostEvent::Clicked(id) if id == "pane-7" => clicked = true,
                _ => {}
            }
        }
        assert!(saw_visible, "pushing a toast must emit Visibility(true)");
        assert!(clicked, "clicking toast must emit Clicked(pane-7)");
    }

    #[test]
    fn overlay_size_tracks_measured_height_without_dead_zone_floor() {
        assert_eq!(compute_overlay_size(false, 44.0), None);
        assert_eq!(
            compute_overlay_size(true, 44.0),
            Some((
                TOAST_STACK_WIDTH + TOAST_OVERLAY_PAD * 2.0,
                44.0 + TOAST_OVERLAY_PAD * 2.0,
            ))
        );
    }
}
