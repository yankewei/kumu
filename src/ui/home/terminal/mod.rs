//! One terminal pane: a project's shell, with chrome for split / close.
//!
//! Painting follows the herdr/Ghostty path: merge same-color backgrounds and
//! same-style text into runs, force glyphs onto the cell grid, draw box /
//! Powerline / braille as geometry ([`super::sprites`]), fall back to the
//! bundled Nerd symbols font for missing icons, and reuse a row's shaped ops
//! while its [`Arc`] is unchanged. IME composition (Pinyin and friends) goes
//! through [`EntityInputHandler`]: the half-typed text is previewed in the
//! cursor cell and the system candidate window anchors there too.

mod chrome;
mod drop;
mod input;
mod paint;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::*;

#[cfg(test)]
pub(crate) use self::chrome::FindBarSnapshot;
pub(crate) use self::chrome::PaneChromeSnapshot;
pub(crate) use self::chrome::short_cwd as short_cwd_display;
use self::chrome::{find_bar_snapshot, header_name, header_note, short_cwd};
use self::paint::{GridPaint, RowOp, Typeface, cell_size, grid_origin, paint_color, paint_grid};
use crate::agent::AgentState;
use crate::agent::launch::PaneLaunch;
use crate::app::layout::PaneId;
use crate::app::{Workspace, WorkspaceEvent};
use crate::term::link;
use crate::term::{Cell as TermCell, MouseAction, Rgb, SelectStep, Snapshot, Term};
use crate::theme::{self, KumuTheme, ThemeRevision};

/// Chrome / focus messages from a pane up to the home window.
pub enum TerminalEvent {
    /// The user clicked into this pane; the home window should focus it.
    Activated,
    /// Pane header metadata or find-bar state changed; refresh the NSH shell.
    ChromeChanged,
}

/// Inset around the cell grid, matching the padded look of a pane TUI.
const PAD_X: f32 = 14.;
const PAD_Y: f32 = 10.;

/// Minimum interval between PTY resizes while a left/right workspace drawer is
/// sliding open or closed (~35Hz live adaptive reflow without flooding agent
/// TUIs with 120Hz `SIGWINCH` bursts).
const PANEL_RESIZE_PACE: Duration = Duration::from_millis(28);

/// Grace window after an exited agent pane falls back to a fresh login shell,
/// dropping trailing `Ctrl+C` taps meant for the exiting agent so they do not
/// send `SIGINT` while the new shell is sourcing `~/.zshrc`.
const POST_EXIT_CTRL_C_GUARD: Duration = Duration::from_millis(600);

/// Cached drawing for one viewport, keyed by the snapshot's row Arcs.
struct PaintCache {
    cell: (Pixels, Pixels),
    font: (SharedString, u32),
    bg: Rgb,
    rows: Vec<(Arc<Vec<TermCell>>, Vec<RowOp>)>,
}

/// Where the last paint put the cursor cell, for the IME candidate window.
#[derive(Clone, Copy)]
struct CursorAt {
    bounds: Option<Bounds<Pixels>>,
    cell: (Pixels, Pixels),
}

/// Grid geometry from the last paint, for mapping pointer → cell.
#[derive(Clone, Copy)]
struct GridAt {
    origin: Point<Pixels>,
    cell: (Pixels, Pixels),
    /// Visible grid height in pixels (cell_h * rows), for selection geometry.
    height: f32,
    scale: f32,
    /// The canvas the grid was painted into: a pointer outside it (over the
    /// header or the find bar) is not over any cell.
    bounds: Bounds<Pixels>,
}

pub struct TerminalView {
    pane: PaneId,
    workspace: Entity<Workspace>,
    project: Option<i64>,
    /// Whether the workspace currently shows this pane's terminal here. A
    /// project row click re-shows every pane, and an already shown one must
    /// keep its find bar and painted frame.
    shown: bool,
    snapshot: Snapshot,
    focus: FocusHandle,
    /// Whether the home window treats this pane as the focused one.
    active: bool,
    /// Hidden panes do not expose close controls.
    can_close: bool,
    /// Whether this pane belongs to a split layout and can be zoomed.
    can_zoom: bool,
    /// Whether this pane is currently maximized over its split siblings.
    zoomed: bool,
    /// Theme revision last pushed into the live terminals.
    theme_rev: u64,
    /// IME composition preview ("nihao" before 你好).
    marked: Option<String>,
    /// Active agent state for the pane header's status pill.
    agent_status: Option<AgentState>,
    cursor_at: Rc<Cell<CursorAt>>,
    grid_at: Rc<Cell<GridAt>>,
    /// Left-button selection drag in progress.
    selecting: bool,
    /// Terminal handle that owns the in-flight selection drag, captured on
    /// press so switching projects or losing focus still resets the original
    /// terminal's gesture and autoscroll timer.
    drag_term: Option<Term>,
    /// Latest pointer position while `selecting`, used to re-apply the drag
    /// endpoint after manual wheel/trackpad scrolling.
    drag_pos: Option<Point<Pixels>>,
    /// Window deactivation listener so swallowed mouse-up events (⌘Tab, system
    /// modals) cannot leave a selection drag or autoscroll running.
    window_activation_sub: Option<Subscription>,
    /// Deadline until which trailing `Ctrl+C` is dropped after an agent exits
    /// back into a fresh shell, preventing `SIGINT` from aborting `~/.zshrc`.
    suppress_ctrl_c_until: Option<Instant>,
    /// The link under a ⌘-hovered cell: where it sits and where a click goes.
    link: Option<link::ResolvedLink>,
    /// The cell the pointer is over while ⌘ is held.
    hover: Option<(u16, u16)>,
    /// That cell's row as it looked when the link was resolved. A new frame
    /// that repaints the row refreshes the link, so a scrolling screen never
    /// leaves an underline (or a click target) on old text.
    hover_row: Option<Arc<Vec<TermCell>>>,
    /// The current link came from an OSC 8 read, or one is on its way.
    hover_osc8: bool,
    /// Id of the newest probe: a late OSC 8 answer for an older cell is
    /// dropped instead of replacing the link under the pointer now.
    hover_probe: u64,
    /// Buttons the mouse-tracking program was told about and has not released.
    /// A gesture is decided when the button goes down, so a modifier or mode
    /// change mid-drag cannot drop its Release.
    reported: Vec<gpui::MouseButton>,
    /// Accumulated scroll remainder (touchpad line fractions).
    scrolled: f32,
    /// The last size sent to the process on screen, so a repaint does not
    /// resize again. Cleared when the attached project changes.
    sent: Rc<Cell<(u16, u16, u32, u32)>>,
    /// Timestamp of the last PTY resize, used to pace live reflows during
    /// left/right drawer slide animations.
    last_resize_at: Rc<Cell<Option<Instant>>>,
    /// True while a left/right workspace panel is actively sliding open/closed.
    panel_animating: bool,
    painted: Rc<RefCell<PaintCache>>,
    /// Open find bar state, if any. The UI itself renders in Pure Rust NSH.
    find: Option<FindState>,
    /// Monotonic counter bumped whenever `⌘F` requests input focus.
    find_focus_nonce: u64,
}

/// The find bar's needle state; its input lives in the NSH pane overlay.
struct FindState {
    sent: String,
    focus_nonce: u64,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    pub fn new(pane: PaneId, workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&workspace, |this, _, ev, cx| {
            if let WorkspaceEvent::Frame(pane) = ev
                && *pane == this.pane.0
            {
                this.pull(cx);
            }
        })
        .detach();
        let focus = cx.focus_handle();
        Self {
            pane,
            workspace,
            project: None,
            shown: false,
            snapshot: Snapshot::blank("", ""),
            focus,
            active: true,
            can_close: false,
            can_zoom: false,
            zoomed: false,
            theme_rev: u64::MAX,
            marked: None,
            agent_status: None,
            cursor_at: Rc::new(Cell::new(CursorAt {
                bounds: None,
                cell: (px(0.), px(0.)),
            })),
            grid_at: Rc::new(Cell::new(GridAt {
                origin: point(px(0.), px(0.)),
                cell: (px(0.), px(0.)),
                height: 0.,
                scale: 1.,
                bounds: Bounds::new(point(px(0.), px(0.)), size(px(0.), px(0.))),
            })),
            selecting: false,
            drag_term: None,
            drag_pos: None,
            window_activation_sub: None,
            suppress_ctrl_c_until: None,
            link: None,
            hover: None,
            hover_row: None,
            hover_osc8: false,
            hover_probe: 0,
            reported: Vec::new(),
            scrolled: 0.,
            sent: Rc::new(Cell::new((0, 0, 0, 0))),
            last_resize_at: Rc::new(Cell::new(None)),
            panel_animating: false,
            painted: Rc::new(RefCell::new(PaintCache {
                cell: (px(0.), px(0.)),
                font: (SharedString::from(""), 0),
                bg: Rgb(0, 0, 0),
                rows: Vec::new(),
            })),
            find: None,
            find_focus_nonce: 0,
        }
    }

    pub fn chrome_snapshot(&self) -> PaneChromeSnapshot {
        PaneChromeSnapshot {
            id: self.pane,
            name: header_name(&self.snapshot),
            cwd: short_cwd(&self.snapshot.cwd),
            note: header_note(&self.snapshot),
            agent_status: self.agent_status,
            active: self.active,
            can_close: self.can_close,
            can_zoom: self.can_zoom,
            zoomed: self.zoomed,
            find: self
                .find
                .as_ref()
                .map(|bar| find_bar_snapshot(&self.snapshot, &bar.sent, bar.focus_nonce)),
        }
    }

    pub fn project_id(&self) -> Option<i64> {
        self.project
    }

    /// Set the chrome state of a visible pane.
    pub fn set_chrome(
        &mut self,
        active: bool,
        can_close: bool,
        can_zoom: bool,
        zoomed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.active == active
            && self.can_close == can_close
            && self.can_zoom == can_zoom
            && self.zoomed == zoomed
        {
            return;
        }
        self.active = active;
        self.can_close = can_close;
        self.can_zoom = can_zoom;
        self.zoomed = zoomed;
        cx.notify();
    }

    /// The state the pane header shows beside the path, or `None` for a shell.
    pub fn set_agent_status(&mut self, status: Option<AgentState>, cx: &mut Context<Self>) {
        if self.agent_status == status {
            return;
        }
        self.agent_status = status;
        cx.notify();
    }

    /// Pace live PTY resizes while a surrounding workspace drawer slides open
    /// or closed, then flush the exact final column count once it settles.
    pub fn set_panel_animating(&mut self, animating: bool, cx: &mut Context<Self>) {
        if self.panel_animating == animating {
            return;
        }
        self.panel_animating = animating;
        if !animating {
            cx.notify();
        }
    }

    fn sync_theme(&mut self, cx: &mut Context<Self>) {
        let rev = cx.default_global::<ThemeRevision>().0;
        if rev == self.theme_rev {
            return;
        }
        self.theme_rev = rev;
        let colors = theme::term_colors(cx);
        self.workspace
            .update(cx, |workspace, _| workspace.set_terminal_colors(colors));
        // Default cells change with the theme; drop shaped rows.
        self.painted.borrow_mut().rows.clear();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Show `project_id`, starting `launch` if nothing runs in this pane yet.
    pub fn show(&mut self, project_id: i64, launch: &PaneLaunch, cx: &mut Context<Self>) {
        // Selecting the project that is already shown is a no-op: the terminal
        // and its find bar stay exactly where they are. A failed spawn is the
        // exception — selecting the project again retries it, as it did before.
        if self.shown && self.project == Some(project_id) && self.snapshot.spawn_error.is_none() {
            return;
        }
        let pane = self.pane.0;
        let snapshot = self.workspace.update(cx, |workspace, cx| {
            workspace.show_terminal(pane, project_id, launch, cx)
        });
        self.attach(project_id, snapshot, cx);
    }

    /// Keep this pane's process while its project is hidden.
    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.reset_gesture();
        self.suppress_ctrl_c_until = None;
        self.shown = false;
        self.find = None;
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.terminal_search_clear(pane);
            workspace.hide_terminal(pane);
        });
    }

    /// Briefly suppress trailing `Ctrl+C` after an exited agent pane falls back
    /// to a fresh login shell so exit taps do not abort shell rc startup.
    pub fn guard_post_exit_ctrl_c(&mut self) {
        self.suppress_ctrl_c_until = Some(Instant::now() + POST_EXIT_CTRL_C_GUARD);
    }

    /// Attach a terminal already assigned to this pane by the workspace.
    /// Notification navigation reattaches the process that sent the request.
    pub fn attach(&mut self, project_id: i64, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.reset_gesture();
        self.suppress_ctrl_c_until = None;
        // A find bar belongs to the terminal it was opened on. Reattaching
        // the pane's own terminal (clicking the selected project, a
        // notification from the same pane) keeps it; a different terminal
        // starts without one.
        if self.project != Some(project_id) {
            self.find = None;
            let pane = self.pane.0;
            self.workspace
                .update(cx, |workspace, _| workspace.terminal_search_clear(pane));
        }
        self.shown = true;
        self.project = Some(project_id);
        self.sent.set((0, 0, 0, 0));
        self.marked = None;
        self.painted.borrow_mut().rows.clear();
        self.sync_theme(cx);
        self.snapshot = snapshot;
        cx.notify();
    }

    /// Abandon any mouse gesture in flight (when switching projects, hiding a
    /// pane, or losing window activation). Resets the original `drag_term`'s
    /// selection gesture so its autoscroll timer stops even if this pane now
    /// points at another terminal, and clears `reported` mouse buttons so a
    /// swallowed release cannot lock out future clicks.
    fn reset_gesture(&mut self) {
        if let Some(term) = self.drag_term.take() {
            term.reset_selection_gesture();
        }
        self.selecting = false;
        self.drag_pos = None;
        self.reported.clear();
        self.scrolled = 0.;
        self.link = None;
        self.hover = None;
        self.hover_row = None;
        self.hover_osc8 = false;
        self.hover_probe = self.hover_probe.wrapping_add(1);
    }

    fn pull(&mut self, cx: &mut Context<Self>) {
        if self.project.is_none() {
            return;
        }
        let pane = self.pane.0;
        self.workspace
            .update(cx, |workspace, _| workspace.terminal_wake_handled(pane));
        // Read once without spinning: any frame produced concurrently already
        // queued another `WorkspaceEvent::Frame`.
        let Some(snapshot) = self.workspace.read(cx).terminal_snapshot(pane) else {
            return;
        };
        let prev_chrome = (
            self.snapshot.title.clone(),
            self.snapshot.cwd.clone(),
            self.snapshot.exited,
            self.snapshot.spawn_error.clone(),
            self.snapshot.search.clone(),
        );
        self.snapshot = snapshot;
        // Frames cross from the session thread asynchronously: a snapshot can
        // still carry the previous needle's matches after the input moved on.
        if let Some(sent) = self.find.as_ref().map(|bar| bar.sent.clone())
            && self
                .snapshot
                .search
                .as_ref()
                .is_some_and(|search| search.needle != sent)
        {
            self.snapshot.search = None;
        }
        // The link under the pointer belongs to the row that was painted when
        // the pointer last moved. Output, a scroll or a resize can put other
        // text there; repaint the underline on the new row instead of leaving
        // it (and a click target) on stale cells.
        if let Some((col, row)) = self.hover
            && let Some(cells) = self.snapshot.cells.get(usize::from(row))
            && self
                .hover_row
                .as_ref()
                .is_none_or(|old| !Arc::ptr_eq(old, cells))
        {
            self.refresh_link(col, row, cx);
        }
        let next_chrome = (
            &self.snapshot.title,
            &self.snapshot.cwd,
            self.snapshot.exited,
            &self.snapshot.spawn_error,
            &self.snapshot.search,
        );
        if (
            &prev_chrome.0,
            &prev_chrome.1,
            prev_chrome.2,
            &prev_chrome.3,
            &prev_chrome.4,
        ) != next_chrome
        {
            cx.emit(TerminalEvent::ChromeChanged);
        }
        cx.notify();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.window_activation_sub.is_none() {
            self.window_activation_sub =
                Some(cx.observe_window_activation(window, |this, window, _| {
                    if !window.is_window_active() {
                        this.reset_gesture();
                    }
                }));
        }
        self.sync_theme(cx);
        let theme = KumuTheme::global(cx);
        let colors = &theme.colors;
        let radius = px(theme.radius);
        let fg = colors.foreground;
        let search_hit = colors.selection.opacity(0.55);
        let search_active = colors.warning.opacity(0.55);
        let drop_accent = colors.blue;
        let drop_tint = colors.blue.opacity(0.08);
        let drop_pill_bg = colors.popover;
        let (font_family, font_px) = theme::term_font(cx);
        let snapshot = self.snapshot.clone();
        let marked = self.marked.clone();
        let workspace = self.workspace.clone();
        let project = self.project;
        let pane_term = self.pane.0;
        let has_project = project.is_some();
        let sent = self.sent.clone();
        let last_resize_at = self.last_resize_at.clone();
        let panel_animating = self.panel_animating;
        let painted = self.painted.clone();
        let cursor_at = self.cursor_at.clone();
        let grid_at = self.grid_at.clone();
        let focused = self.focus.is_focused(window);
        let input_focus = self.focus.clone();
        let input_view = cx.entity();
        let mouse_view = cx.entity().downgrade();
        let pane = self.pane;
        let drop_group: SharedString = format!("pane-drop-{}", pane.0).into();
        let show_drop_overlay = has_project && cx.has_active_drag();
        // The link under a ⌘-hovered cell: its runs are underlined and each is
        // a hitbox, so the pointer turns into a hand over it.
        let link_spans: Vec<link::Span> = self
            .link
            .as_ref()
            .map(|link| link.spans.clone())
            .unwrap_or_default();
        let snapshot_rows = snapshot.rows;
        let link_hitboxes: Rc<Cell<Vec<Hitbox>>> = Rc::new(Cell::new(Vec::new()));
        let link_spans_layout = link_spans.clone();
        let link_hitboxes_layout = link_hitboxes.clone();
        let typeface = Typeface {
            family: font_family,
            size: font_px,
        };
        let typeface_layout = typeface.clone();
        let typeface_paint = typeface;
        let term_bg = paint_color(snapshot.bg);
        let term_id = format!("kumu-term-{}", pane.0);
        let mut container = div()
            .id(term_id)
            .group(drop_group.clone())
            .relative()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(term_bg)
            .track_focus(&self.focus)
            .key_context("Terminal")
            .can_drop(move |drag, _, _| has_project && drag.is::<ExternalPaths>())
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drop_paths(paths, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    cx.emit(TerminalEvent::Activated);
                    // ⌘-click opens the link under the pointer,
                    // like Ghostty on macOS; it is never the
                    // start of a selection.
                    if ev.modifiers.platform && this.open_link(ev.position, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    if this.press_mouse(ev.button, ev.position, &ev.modifiers, cx) {
                        if this.project.is_some() {
                            let pane = this.pane.0;
                            this.workspace.update(cx, |workspace, _| {
                                workspace.terminal_clear_selection(pane);
                            });
                        }
                    } else if !this.reported.contains(&ev.button) {
                        this.start_selection(ev.position, cx);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    cx.emit(TerminalEvent::Activated);
                    if !this.press_mouse(ev.button, ev.position, &ev.modifiers, cx) {
                        this.paste(cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    cx.emit(TerminalEvent::Activated);
                    this.press_mouse(ev.button, ev.position, &ev.modifiers, cx);
                }),
            )
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                this.on_scroll(ev, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let (cell_w, cell_h) = cell_size(window, &typeface_layout);
                        let scale = window.scale_factor();
                        let cols = ((f32::from(bounds.size.width) - 2. * PAD_X) / f32::from(cell_w))
                            .floor()
                            .max(2.) as u16;
                        let rows = ((f32::from(bounds.size.height) - 2. * PAD_Y)
                            / f32::from(cell_h))
                        .floor()
                        .max(1.) as u16;
                        let cell_px = (
                            (f32::from(cell_w) * scale).round().max(1.) as u32,
                            (f32::from(cell_h) * scale).round().max(1.) as u32,
                        );
                        let next = (cols, rows, cell_px.0, cell_px.1);
                        let prev = sent.get();
                        if prev != next {
                            let now = Instant::now();
                            let ready = !panel_animating
                                || prev.0 == 0
                                || last_resize_at
                                    .get()
                                    .is_none_or(|at| now.duration_since(at) >= PANEL_RESIZE_PACE);
                            if ready {
                                sent.set(next);
                                last_resize_at.set(Some(now));
                                if has_project {
                                    workspace.update(cx, |workspace, _| {
                                        workspace.terminal_resize(
                                            pane_term, cols, rows, cell_px.0, cell_px.1,
                                        )
                                    });
                                }
                            }
                        }
                        let origin =
                            grid_origin(bounds, cell_h, snapshot_rows, window.scale_factor());
                        let hitboxes = link_spans_layout
                            .iter()
                            .filter(|span| span.to > span.from && span.row < snapshot_rows)
                            .map(|span| {
                                let at = origin
                                    + point(
                                        cell_w * f32::from(span.from),
                                        cell_h * f32::from(span.row),
                                    );
                                window.insert_hitbox(
                                    Bounds::new(
                                        at,
                                        size(cell_w * f32::from(span.to - span.from), cell_h),
                                    ),
                                    HitboxBehavior::Normal,
                                )
                            })
                            .collect();
                        link_hitboxes_layout.set(hitboxes);
                        (cell_w, cell_h)
                    },
                    move |bounds, cell, window, cx| {
                        for hitbox in link_hitboxes.take() {
                            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                        }
                        let origin = paint_grid(
                            GridPaint {
                                bounds,
                                snapshot: &snapshot,
                                cell,
                                focused,
                                marked: marked.as_deref(),
                                typeface: &typeface_paint,
                                painted: &painted,
                                cursor_at: &cursor_at,
                                search_hit,
                                search_active,
                                link: &link_spans,
                            },
                            window,
                            cx,
                        );
                        grid_at.set(GridAt {
                            origin,
                            cell,
                            height: f32::from(cell.1) * snapshot.rows as f32,
                            scale: window.scale_factor(),
                            bounds,
                        });
                        let view = mouse_view.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let _ = view.update(cx, |this, cx| {
                                if this.selecting {
                                    if ev.pressed_button == Some(MouseButton::Left) {
                                        this.drag_pos = Some(ev.position);
                                        this.apply_select(SelectStep::Drag, ev.position, cx);
                                    } else {
                                        this.finish_selection(ev.position, cx);
                                    }
                                    return;
                                }
                                if let Some(button) = ev.pressed_button
                                    && this.reported.contains(&button)
                                {
                                    this.send_mouse(
                                        MouseAction::Motion,
                                        Some(button),
                                        ev.position,
                                        &ev.modifiers,
                                        cx,
                                    );
                                    return;
                                }
                                // Links only highlight while
                                // ⌘ is held; the modifier
                                // release itself arrives
                                // with the next move.
                                this.hover_link(ev.position, &ev.modifiers, cx);
                            });
                        });
                        let view = mouse_view.clone();
                        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let _ = view.update(cx, |this, cx| {
                                this.release_mouse(ev.button, ev.position, &ev.modifiers, cx);
                                if this.selecting && ev.button == MouseButton::Left {
                                    this.finish_selection(ev.position, cx);
                                    cx.stop_propagation();
                                }
                            });
                        });
                        // ⌘ can be pressed or released while
                        // the pointer stands still: neither
                        // produces a mouse move of its own. The
                        // window-level listener reaches every
                        // pane, not just the focused one.
                        let view = mouse_view.clone();
                        window.on_modifiers_changed(
                            move |ev: &ModifiersChangedEvent, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.hover_link(window.mouse_position(), &ev.modifiers, cx);
                                });
                            },
                        );
                        if focused {
                            window.handle_input(
                                &input_focus,
                                ElementInputHandler::new(bounds, input_view.clone()),
                                cx,
                            );
                        }
                    },
                )
                .size_full(),
            );

        if show_drop_overlay {
            container = container.child(
                div()
                    .id(format!("pane-drop-overlay-{}", pane.0))
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(0.)
                    .group_drag_over::<ExternalPaths>(drop_group, move |style| {
                        style
                            .opacity(1.)
                            .bg(drop_tint)
                            .border_2()
                            .border_color(drop_accent)
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .px_3()
                            .py(px(6.))
                            .rounded(radius)
                            .bg(drop_pill_bg)
                            .border_1()
                            .border_color(drop_accent)
                            .shadow_sm()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg)
                            .child(
                                svg()
                                    .path("icons/file.svg")
                                    .size(px(14.))
                                    .text_color(drop_accent),
                            )
                            .child("松开以插入路径"),
                    ),
            );
        }

        container
    }
}
