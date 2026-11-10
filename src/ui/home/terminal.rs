//! One terminal pane: a session's TUI, with chrome for split / close.
//!
//! Painting follows the herdr/Ghostty path: merge same-color backgrounds and
//! same-style text into runs, force glyphs onto the cell grid, draw box /
//! Powerline / braille as geometry ([`super::sprites`]), fall back to the
//! bundled Nerd symbols font for missing icons, and reuse a row's shaped ops
//! while its [`Arc`] is unchanged. IME composition (Pinyin and friends) goes
//! through [`EntityInputHandler`].

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::panes::PaneId;
use crate::agent::AGENTS;
use crate::app::{Workspace, WorkspaceEvent};
use crate::term::{
    self, Cell as TermCell, CursorShape, KeyInput, MouseAction, MouseButton as TermMouseButton,
    MouseInput, MouseMods, Rgb, SelectStep, Snapshot,
};
use crate::theme::{self, ThemeRevision};
use crate::ui::tooltip::{HoverLabel as _, Tooltips};
use crate::ui::widgets::agent_logo;

/// Chrome / focus messages from a pane up to the home window.
pub enum TerminalEvent {
    /// The user clicked into this pane; the home window should focus it.
    Activated,
    /// Split this pane to the right (another shell under the same session).
    SplitRight,
    /// Split this pane downward.
    SplitDown,
    /// Close this pane and its shell; the session row goes when it was the last pane.
    Close,
}

/// Inset around the cell grid, matching the padded look of a pane TUI.
const PAD_X: f32 = 14.;
const PAD_Y: f32 = 10.;

/// Cached drawing for one viewport, keyed by the snapshot's row Arcs.
struct PaintCache {
    cell: (Pixels, Pixels),
    font: (SharedString, u32),
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
}

pub struct TerminalView {
    pane: PaneId,
    workspace: Entity<Workspace>,
    /// Hover labels for the pane chrome buttons.
    tooltips: Entity<Tooltips>,
    session: Option<i64>,
    snapshot: Snapshot,
    focus: FocusHandle,
    /// Whether the home window treats this pane as the focused one.
    active: bool,
    /// Close is only meaningful when more than one pane is open.
    can_close: bool,
    /// Theme revision last pushed into the live terminals.
    theme_rev: u64,
    /// IME composition preview ("nihao" before 你好).
    marked: Option<String>,
    cursor_at: Rc<Cell<CursorAt>>,
    grid_at: Rc<Cell<GridAt>>,
    /// Left-button selection drag in progress.
    selecting: bool,
    /// Buttons the mouse-tracking program was told about and has not released.
    /// A gesture is decided when the button goes down, so a modifier or mode
    /// change mid-drag cannot drop its Release.
    reported: Vec<gpui::MouseButton>,
    /// Accumulated scroll remainder (touchpad line fractions).
    scrolled: f32,
    /// The last size sent to the process on screen, so a repaint does not
    /// resize again. Cleared when the session changes.
    sent: Rc<Cell<(u16, u16, u32, u32)>>,
    painted: Rc<RefCell<PaintCache>>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    pub fn new(
        pane: PaneId,
        workspace: Entity<Workspace>,
        tooltips: Entity<Tooltips>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            tooltips,
            session: None,
            snapshot: Snapshot::blank("", ""),
            focus,
            active: true,
            can_close: false,
            theme_rev: u64::MAX,
            marked: None,
            cursor_at: Rc::new(Cell::new(CursorAt {
                bounds: None,
                cell: (px(0.), px(0.)),
            })),
            grid_at: Rc::new(Cell::new(GridAt {
                origin: point(px(0.), px(0.)),
                cell: (px(0.), px(0.)),
                height: 0.,
                scale: 1.,
            })),
            selecting: false,
            reported: Vec::new(),
            scrolled: 0.,
            sent: Rc::new(Cell::new((0, 0, 0, 0))),
            painted: Rc::new(RefCell::new(PaintCache {
                cell: (px(0.), px(0.)),
                font: (SharedString::from(""), 0),
                rows: Vec::new(),
            })),
        }
    }

    pub fn session_id(&self) -> Option<i64> {
        self.session
    }

    pub fn set_chrome(&mut self, active: bool, can_close: bool, cx: &mut Context<Self>) {
        if self.active == active && self.can_close == can_close {
            return;
        }
        self.active = active;
        self.can_close = can_close;
        cx.notify();
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

    /// Show `session_id`, starting its TUI if it is not already running.
    pub fn show(&mut self, session_id: i64, cx: &mut Context<Self>) {
        let pane = self.pane.0;
        let snapshot = self.workspace.update(cx, |workspace, cx| {
            workspace.show_terminal(pane, session_id, cx)
        });
        self.attach(session_id, snapshot, cx);
    }

    /// Keep this session's pane state while its session is hidden.
    pub fn hide(&mut self, cx: &mut Context<Self>) {
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.hide_terminal(pane);
        });
    }

    /// Attach a terminal already assigned to this pane by the workspace.
    /// Notification navigation preserves even an exited process's final screen.
    pub fn attach(&mut self, session_id: i64, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.reset_gesture();
        self.session = Some(session_id);
        self.sent.set((0, 0, 0, 0));
        self.marked = None;
        self.painted.borrow_mut().rows.clear();
        self.sync_theme(cx);
        self.snapshot = snapshot;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.drop_terminal(pane);
        });
        self.reset_gesture();
        self.session = None;
        self.sent.set((0, 0, 0, 0));
        self.marked = None;
        self.painted.borrow_mut().rows.clear();
        self.snapshot = Snapshot::blank("", "");
        cx.notify();
    }

    /// A pane that changes session abandons whatever mouse gesture was in
    /// flight. Left alone, `selecting` would make the next plain move end a
    /// selection that belonged to the old session, and a `reported` button
    /// whose release never arrived (a drag that switched panes or lost focus)
    /// would keep this pane from taking any click again.
    fn reset_gesture(&mut self) {
        self.selecting = false;
        self.reported.clear();
        self.scrolled = 0.;
    }

    fn pull(&mut self, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let pane = self.pane.0;
        self.workspace
            .update(cx, |workspace, _| workspace.terminal_wake_handled(pane));
        loop {
            let generation = self.workspace.read(cx).terminal_generation(pane);
            let Some(snapshot) = self.workspace.read(cx).terminal_snapshot(pane) else {
                return;
            };
            self.snapshot = snapshot;
            if self.workspace.read(cx).terminal_generation(pane) == generation {
                break;
            }
        }
        cx.notify();
    }

    fn write_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.session.is_none() || text.is_empty() {
            return;
        }
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.terminal_write(pane, text.as_bytes().to_vec())
        });
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        // IME / character composition: the EntityInputHandler owns these.
        if event.prefer_character_input {
            return;
        }
        let mods = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if mods.platform && !mods.control && !mods.alt && !mods.shift {
            match key {
                "c" => {
                    if self.copy(cx) {
                        cx.stop_propagation();
                    }
                    return;
                }
                "v" => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                // Select-all is not wired yet; don't send ⌘A into the PTY.
                "a" => return,
                _ => return,
            }
        }
        if mods.platform {
            return;
        }
        if self.session.is_none() {
            return;
        }
        let pane = self.pane.0;
        let input = KeyInput {
            key: event.keystroke.key.clone(),
            key_char: event.keystroke.key_char.clone(),
            shift: mods.shift,
            control: mods.control,
            alt: mods.alt,
        };
        self.workspace
            .update(cx, |workspace, _| workspace.terminal_key(pane, input));
        cx.stop_propagation();
    }

    /// Copy the current selection. The text lives on the session thread, so the
    /// read happens on a background task instead of blocking event dispatch.
    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        if self.session.is_none() {
            return false;
        }
        let Some(term) = self.workspace.read(cx).terminal_handle(self.pane.0) else {
            return false;
        };
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_spawn(async move { term.selection_text() })
                .await;
            let Some(text) = text.filter(|text| !text.is_empty()) else {
                return;
            };
            let _ = this.update(cx, |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            });
        })
        .detach();
        true
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let pane = self.pane.0;
        self.workspace
            .update(cx, |workspace, _| workspace.terminal_paste(pane, text));
    }

    /// Whether the running program owns the pointer for a new gesture.
    fn takes_mouse(&self, mods: &Modifiers) -> bool {
        self.session.is_some() && self.snapshot.mouse_tracking && !mods.shift
    }

    /// Hand a button press to the program, latching it until its Release.
    fn press_mouse(
        &mut self,
        button: gpui::MouseButton,
        pos: Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.reported.contains(&button) || !self.takes_mouse(mods) {
            return false;
        }
        self.reported.push(button);
        self.send_mouse(MouseAction::Press, Some(button), pos, mods, cx);
        true
    }

    /// Release a button the program was told about, whatever the modifiers are
    /// now, so a modifier change mid-drag cannot leave it stuck down.
    fn release_mouse(
        &mut self,
        button: gpui::MouseButton,
        pos: Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        if !self.reported.contains(&button) {
            return;
        }
        self.reported.retain(|held| *held != button);
        self.send_mouse(MouseAction::Release, Some(button), pos, mods, cx);
    }

    fn send_mouse(
        &mut self,
        action: MouseAction,
        button: Option<gpui::MouseButton>,
        pos: Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        let pane = self.pane.0;
        let grid = self.grid_at.get();
        let (x, y) = local_pos(grid, pos);
        let input = MouseInput {
            action,
            button: button.and_then(map_mouse_button),
            x: x * grid.scale,
            y: y * grid.scale,
            cell_w: f32::from(grid.cell.0) * grid.scale,
            cell_h: f32::from(grid.cell.1) * grid.scale,
            any_pressed: !self.reported.is_empty(),
            mods: mouse_mods(mods),
        };
        self.workspace
            .update(cx, |workspace, _| workspace.terminal_mouse(pane, input));
    }

    fn start_selection(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.selecting = true;
        self.apply_select(SelectStep::Press, pos, cx);
    }

    /// End a selection drag: copy what the gesture selected. The Release step
    /// and the text both live on the session thread, so it runs off-thread.
    fn finish_selection(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.session.is_none() {
            return;
        }
        let Some(point) = self.select_point(pos) else {
            return;
        };
        let Some(term) = self.workspace.read(cx).terminal_handle(self.pane.0) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_spawn(async move {
                    term.select(
                        SelectStep::Release,
                        point.col,
                        point.row,
                        point.x,
                        point.y,
                        point.cell_w,
                        point.height,
                    )
                })
                .await;
            let Some(text) = text.filter(|text| !text.is_empty()) else {
                return;
            };
            let _ = this.update(cx, |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            });
        })
        .detach();
    }

    fn apply_select(&mut self, step: SelectStep, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let Some(point) = self.select_point(pos) else {
            return;
        };
        if let Some(term) = self.workspace.read(cx).terminal_handle(self.pane.0) {
            // Press and Drag never wait on the session thread.
            let _ = term.select(
                step,
                point.col,
                point.row,
                point.x,
                point.y,
                point.cell_w,
                point.height,
            );
        }
        cx.notify();
    }

    fn select_point(&self, pos: Point<Pixels>) -> Option<SelectPoint> {
        let grid = self.grid_at.get();
        let (x, y) = local_pos(grid, pos);
        let (cw, ch) = (f32::from(grid.cell.0), f32::from(grid.cell.1));
        if cw <= 0. || ch <= 0. {
            return None;
        }
        Some(SelectPoint {
            col: (x / cw) as u16,
            row: (y / ch) as u16,
            x,
            y,
            cell_w: cw,
            height: grid.height.max(ch),
        })
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let pane = self.pane.0;
        cx.stop_propagation();
        let grid = self.grid_at.get();
        let (cw, ch) = (f32::from(grid.cell.0), f32::from(grid.cell.1));
        if ch <= 0. {
            return;
        }
        let lines = match event.delta {
            ScrollDelta::Lines(p) => {
                if cfg!(target_os = "macos") {
                    macos_wheel_steps(p.y)
                } else {
                    p.y as i32
                }
            }
            ScrollDelta::Pixels(p) => {
                // Whole lines go to the terminal; the sub-line remainder stays
                // in the accumulator so a slow trackpad still scrolls.
                self.scrolled += f32::from(p.y) / ch;
                let lines = self.scrolled.trunc() as i32;
                self.scrolled -= lines as f32;
                lines
            }
        };
        if lines == 0 {
            return;
        }
        // GPUI: positive y is content moving down = scroll up in terminal terms.
        let delta = -(lines as isize);
        let (x, y) = local_pos(grid, event.position);
        let mods = mouse_mods(&event.modifiers);
        self.workspace.update(cx, |workspace, _| {
            workspace.terminal_scroll(
                pane,
                delta,
                x * grid.scale,
                y * grid.scale,
                cw * grid.scale,
                ch * grid.scale,
                mods,
            )
        });
        cx.notify();
    }
}

/// One gesture position in grid-local logical pixels.
#[derive(Clone, Copy)]
struct SelectPoint {
    col: u16,
    row: u16,
    x: f32,
    y: f32,
    cell_w: f32,
    height: f32,
}

fn mouse_mods(mods: &Modifiers) -> MouseMods {
    MouseMods {
        shift: mods.shift,
        control: mods.control,
        alt: mods.alt,
    }
}

fn local_pos(grid: GridAt, pos: Point<Pixels>) -> (f32, f32) {
    (
        f32::from(pos.x - grid.origin.x).max(0.),
        f32::from(pos.y - grid.origin.y).max(0.),
    )
}

fn map_mouse_button(button: gpui::MouseButton) -> Option<TermMouseButton> {
    match button {
        gpui::MouseButton::Left => Some(TermMouseButton::Left),
        gpui::MouseButton::Right => Some(TermMouseButton::Right),
        gpui::MouseButton::Middle => Some(TermMouseButton::Middle),
        _ => None,
    }
}

fn macos_wheel_steps(ticks: f32) -> i32 {
    if ticks == 0. {
        return 0;
    }
    (ticks.signum() * ticks.abs().max(1.) * 3.).trunc() as i32
}

/// OS text input (Pinyin, Japanese, Korean, dead keys) types here. Native
/// composition previews reach this handler; finished text is written to the PTY.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked.clone().unwrap_or_default();
        let units: Vec<u16> = marked.encode_utf16().collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *adjusted = Some(range.clone());
        Some(String::from_utf16_lossy(&units[range]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self.marked.as_ref().map_or(0, |m| m.encode_utf16().count());
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| 0..m.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.write_text(text, cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!text.is_empty()).then(|| text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _element: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let at = self.cursor_at.get();
        let bounds = at.bounds?;
        Some(Bounds::new(
            bounds.origin + point(at.cell.0 * range.start as f32, px(0.)),
            bounds.size,
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_theme(cx);
        let theme = cx.theme();
        let (font_family, font_px) = theme::term_font(cx);
        let snapshot = self.snapshot.clone();
        let marked = self.marked.clone();
        let workspace = self.workspace.clone();
        let session = self.session;
        let pane_term = self.pane.0;
        let has_session = session.is_some();
        let sent = self.sent.clone();
        let painted = self.painted.clone();
        let cursor_at = self.cursor_at.clone();
        let grid_at = self.grid_at.clone();
        let focused = self.focus.is_focused(window);
        let input_focus = self.focus.clone();
        let input_view = cx.entity();
        let mouse_view = cx.entity().downgrade();
        let (mark, name, cwd) = header_identity(session, &snapshot, &self.workspace, cx);
        let note = header_note(&snapshot);
        let active = self.active;
        let can_close = self.can_close;
        let pane = self.pane;
        let tooltips = self.tooltips.clone();
        let border = if active {
            theme.border
        } else {
            theme.transparent
        };
        let header_bg = if active {
            theme.secondary.opacity(0.45)
        } else {
            theme.transparent
        };
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let warning = theme.warning;
        let radius = theme.radius;
        let hover = theme.accent.opacity(0.5);
        let header = terminal_header(
            pane, mark, name, cwd, note, header_bg, can_close, muted, fg, warning, radius, hover,
            &tooltips, cx,
        );
        let typeface = Typeface {
            family: font_family,
            size: font_px,
        };
        let typeface_layout = typeface.clone();
        let typeface_paint = typeface;
        let term_bg = paint_color(snapshot.bg);
        let term_id = format!("kumu-term-{}", pane.0);
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .border_1()
            .border_color(border)
            .bg(term_bg)
            .child(header)
            .child(
                div()
                    .id(term_id)
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .size_full()
                    .track_focus(&self.focus)
                    .key_context("Terminal")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            this.focus.focus(window, cx);
                            cx.emit(TerminalEvent::Activated);
                            if this.press_mouse(ev.button, ev.position, &ev.modifiers, cx) {
                                if this.session.is_some() {
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
                    .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                        this.on_scroll(ev, cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        this.on_key(event, cx);
                    }))
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                let (cell_w, cell_h) = cell_size(window, &typeface_layout);
                                let scale = window.scale_factor();
                                let cols = ((f32::from(bounds.size.width) - 2. * PAD_X)
                                    / f32::from(cell_w))
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
                                if sent.get() != next {
                                    sent.set(next);
                                    if has_session {
                                        workspace.update(cx, |workspace, _| {
                                            workspace.terminal_resize(
                                                pane_term, cols, rows, cell_px.0, cell_px.1,
                                            )
                                        });
                                    }
                                }
                                (cell_w, cell_h)
                            },
                            move |bounds, cell, window, cx| {
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
                                    },
                                    window,
                                    cx,
                                );
                                grid_at.set(GridAt {
                                    origin,
                                    cell,
                                    height: f32::from(cell.1) * snapshot.rows as f32,
                                    scale: window.scale_factor(),
                                });
                                let view = mouse_view.clone();
                                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                                    if phase != DispatchPhase::Capture {
                                        return;
                                    }
                                    let _ = view.update(cx, |this, cx| {
                                        if this.selecting {
                                            if ev.pressed_button == Some(MouseButton::Left) {
                                                this.apply_select(
                                                    SelectStep::Drag,
                                                    ev.position,
                                                    cx,
                                                );
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
                                        }
                                    });
                                });
                                let view = mouse_view.clone();
                                window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                                    if phase != DispatchPhase::Capture {
                                        return;
                                    }
                                    let _ = view.update(cx, |this, cx| {
                                        this.release_mouse(
                                            ev.button,
                                            ev.position,
                                            &ev.modifiers,
                                            cx,
                                        );
                                        if this.selecting && ev.button == MouseButton::Left {
                                            this.finish_selection(ev.position, cx);
                                            cx.stop_propagation();
                                        }
                                    });
                                });
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
                    ),
            )
    }
}

#[derive(Clone)]
struct Typeface {
    family: SharedString,
    size: f32,
}

/// Top bar like the reference pane chrome: mark, name, shortened cwd, split /
/// close actions.
#[allow(clippy::too_many_arguments)]
fn terminal_header(
    pane: PaneId,
    mark: AnyElement,
    name: String,
    cwd: String,
    note: Option<String>,
    header_bg: Hsla,
    can_close: bool,
    muted: Hsla,
    fg: Hsla,
    warning: Hsla,
    radius: Pixels,
    hover: Hsla,
    tooltips: &Entity<Tooltips>,
    cx: &mut Context<TerminalView>,
) -> impl IntoElement {
    let path = short_cwd(&cwd);

    h_flex()
        .id(format!("pane-head-{}", pane.0))
        .flex_none()
        .items_center()
        .px_3()
        .py_1p5()
        .gap_2()
        .min_w_0()
        .bg(header_bg)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.focus.focus(window, cx);
                cx.emit(TerminalEvent::Activated);
                cx.notify();
            }),
        )
        .child(div().size(px(14.)).flex_none().child(mark))
        .child(
            div()
                .flex_none()
                .text_xs()
                .font_medium()
                .text_color(fg)
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(muted)
                .truncate()
                .child(path),
        )
        .when_some(note, |row, text| {
            row.child(div().flex_none().text_xs().text_color(warning).child(text))
        })
        .child(chrome_button(
            format!("split-right-{}", pane.0),
            "split-right.svg",
            "向右分屏",
            muted,
            radius,
            hover,
            tooltips,
            cx.listener(|_, _, _, cx| {
                cx.emit(TerminalEvent::SplitRight);
                cx.stop_propagation();
            }),
        ))
        .child(chrome_button(
            format!("split-down-{}", pane.0),
            "split-down.svg",
            "向下分屏",
            muted,
            radius,
            hover,
            tooltips,
            cx.listener(|_, _, _, cx| {
                cx.emit(TerminalEvent::SplitDown);
                cx.stop_propagation();
            }),
        ))
        .when(can_close, |row| {
            row.child(chrome_button(
                format!("close-pane-{}", pane.0),
                "icons/close.svg",
                "关闭窗格",
                muted,
                radius,
                hover,
                tooltips,
                cx.listener(|_, _, _, cx| {
                    cx.emit(TerminalEvent::Close);
                    cx.stop_propagation();
                }),
            ))
        })
}

#[allow(clippy::too_many_arguments)]
fn chrome_button(
    id: String,
    icon_path: &'static str,
    label: &'static str,
    muted: Hsla,
    radius: Pixels,
    hover: Hsla,
    tooltips: &Entity<Tooltips>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .rounded(radius)
        .p_0p5()
        .cursor_pointer()
        .text_color(muted)
        .hover(move |style| style.bg(hover))
        .hover_label(tooltips, label)
        .on_click(on_click)
        .child(
            Icon::empty()
                .path(icon_path)
                .with_size(px(14.))
                .text_color(muted),
        )
}

fn header_identity(
    session_id: Option<i64>,
    snapshot: &Snapshot,
    workspace: &Entity<Workspace>,
    cx: &App,
) -> (AnyElement, String, String) {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let session = session_id.and_then(|id| workspace.read(cx).session(id).cloned());
    let agent_id = session
        .as_ref()
        .map(|s| s.agent.as_str())
        .filter(|id| !id.is_empty())
        .unwrap_or(snapshot.command.as_str());
    let cwd = session
        .as_ref()
        .map(|s| s.cwd.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| snapshot.cwd.clone());
    if term::is_shell(agent_id) {
        let name = if snapshot.command.is_empty() {
            "终端".into()
        } else {
            snapshot.command.clone()
        };
        let mark = Icon::new(IconName::SquareTerminal)
            .with_size(px(14.))
            .text_color(muted)
            .into_any_element();
        return (mark, name, cwd);
    }
    let agent = AGENTS.iter().find(|a| a.id == agent_id);
    let name = agent.map(|a| a.name.to_string()).unwrap_or_else(|| {
        if snapshot.command.is_empty() {
            "终端".into()
        } else {
            snapshot.command.clone()
        }
    });
    (
        agent_logo(agent.and_then(|a| a.logo), px(14.), muted),
        name,
        cwd,
    )
}

fn header_note(snapshot: &Snapshot) -> Option<String> {
    if let Some(error) = &snapshot.spawn_error {
        return Some(format!("启动失败：{error}"));
    }
    if snapshot.exited && !snapshot.command.is_empty() {
        return Some("已退出".into());
    }
    if snapshot.command.is_empty() {
        return Some("还没有会话".into());
    }
    None
}

fn short_cwd(cwd: &str) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    if let Some(home) = home.as_ref()
        && let Ok(stripped) = std::path::Path::new(cwd).strip_prefix(home)
    {
        let rest = stripped.to_string_lossy();
        if rest.is_empty() {
            return "~".into();
        }
        return format!("~/{rest}");
    }
    cwd.to_string()
}

fn cell_size(window: &Window, typeface: &Typeface) -> (Pixels, Pixels) {
    let font = cell_font(typeface, false, false);
    let system = window.text_system();
    let id = system.resolve_font(&font);
    let size_px = px(typeface.size);
    let width = system
        .advance(id, size_px, 'M')
        .map(|advance| advance.width)
        .unwrap_or(size_px * 0.6);
    let height = system.ascent(id, size_px) + system.descent(id, size_px).abs();
    // Width stays the font advance; boxes snap edges to device pixels instead.
    let scale = window.scale_factor();
    (width, px((f32::from(height) * 1.2 * scale).ceil() / scale))
}

fn cell_font(typeface: &Typeface, bold: bool, italic: bool) -> Font {
    let mut font = gpui::font(typeface.family.clone());
    font.weight = if bold {
        FontWeight::BOLD
    } else {
        FontWeight::MEDIUM
    };
    font.style = if italic {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    };
    font
}

fn paint_color(rgb: Rgb) -> Hsla {
    Rgba {
        r: rgb.0 as f32 / 255.,
        g: rgb.1 as f32 / 255.,
        b: rgb.2 as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// WCAG relative luminance of an sRGB cell color.
fn luminance(rgb: Rgb) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(rgb.0) + 0.7152 * ch(rgb.1) + 0.0722 * ch(rgb.2)
}

fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Ghostty `minimum-contrast`: keep intentional low-contrast sprites, else
/// lift fg toward near-white / near-black when it would vanish into `bg`.
fn readable_fg(fg: Rgb, bg: Rgb, text: &str) -> Rgb {
    const MINIMUM: f32 = 1.1;
    if super::sprites::is_sprite(text) || preserves_graphics_color(text) {
        return fg;
    }
    if contrast_ratio(fg, bg) >= MINIMUM {
        return fg;
    }
    let light = Rgb(0xf2, 0xf2, 0xf4);
    let dark = Rgb(0x1b, 0x1b, 0x1e);
    if contrast_ratio(light, bg) >= contrast_ratio(dark, bg) {
        light
    } else {
        dark
    }
}

fn preserves_graphics_color(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        matches!(
            c as u32,
            0x2500..=0x259f | 0xe0b0..=0xe0d7 | 0x1fb00..=0x1fbff | 0x1cc00..=0x1cebf
        )
    })
}

/// Force-width shaping moves glyph origins; it does not shrink fallback glyphs
/// (emoji). Scale the font down when a non-wide cell overflows its column.
fn fallback_size(
    c: &TermCell,
    font: &Font,
    width: Pixels,
    size: f32,
    ts: &WindowTextSystem,
) -> Option<Pixels> {
    if c.wide || c.text.is_ascii() {
        return None;
    }
    let first = c.text.chars().next()?;
    let size_px = px(size);
    let primary = ts.resolve_font(font);
    if ts.advance(primary, size_px, first).is_ok() {
        return None;
    }
    let run = TextRun {
        len: c.text.len(),
        font: font.clone(),
        ..Default::default()
    };
    let line = ts.shape_line(c.text.clone().into(), size_px, &[run], None);
    (line.width > width).then(|| size_px * f32::from(width) / f32::from(line.width))
}

fn nerd_icon(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    matches!(
        c as u32,
        0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd
    )
    .then_some(c)
}

enum RowOp {
    Bg { from: usize, to: usize, color: Hsla },
    Sprite { x: usize, text: String, color: Hsla },
    Text { x: usize, line: Box<ShapedLine> },
}

/// Backgrounds merged into runs, then sprites / text: same-style runs forced
/// onto the cell width so glyphs sit on the grid.
fn row_ops(
    row: &[TermCell],
    cell_w: Pixels,
    term_bg: Rgb,
    typeface: &Typeface,
    window: &mut Window,
) -> Vec<RowOp> {
    let mut ops = Vec::new();
    let mut x = 0;
    while x < row.len() {
        let Some(bg) = row[x].bg else {
            x += 1;
            continue;
        };
        let start = x;
        while x < row.len() && row[x].bg == Some(bg) {
            x += 1;
        }
        ops.push(RowOp::Bg {
            from: start,
            to: x,
            color: paint_color(bg),
        });
    }
    let mut x = 0;
    while x < row.len() {
        let start = x;
        let c = &row[start];
        x += 1;
        if c.text.is_empty() && !c.underline && !c.strike {
            continue;
        }
        let cell_bg = c.bg.unwrap_or(term_bg);
        let mut color = paint_color(readable_fg(c.fg, cell_bg, &c.text));
        if c.faint {
            color.a = 0.6;
        }
        if super::sprites::is_sprite(&c.text) {
            ops.push(RowOp::Sprite {
                x: start,
                text: c.text.clone(),
                color,
            });
            continue;
        }
        let mut text_font = cell_font(typeface, c.bold, c.italic);
        let ts = window.text_system();
        // Icons the mono face lacks: shape from the bundled Nerd symbols font.
        if let Some(icon) = nerd_icon(&c.text) {
            let primary = ts.resolve_font(&text_font);
            if ts.advance(primary, px(typeface.size), icon).is_err() {
                text_font = gpui::font(crate::fonts::SYMBOLS_FONT);
                text_font.weight = FontWeight::NORMAL;
            }
        }
        let fitted = fallback_size(c, &text_font, cell_w, typeface.size, ts);
        let mut text = if c.text.is_empty() {
            " ".to_owned()
        } else {
            c.text.clone()
        };
        if !c.wide && fitted.is_none() {
            while x < row.len() {
                let d = &row[x];
                let same = !d.wide
                    && d.fg == c.fg
                    && d.bg == c.bg
                    && d.bold == c.bold
                    && d.italic == c.italic
                    && d.faint == c.faint
                    && d.underline == c.underline
                    && d.strike == c.strike;
                if !same
                    || (d.text.is_empty() && !d.underline && !d.strike)
                    || super::sprites::is_sprite(&d.text)
                    || nerd_icon(&d.text).is_some()
                    || fallback_size(d, &text_font, cell_w, typeface.size, ts).is_some()
                {
                    break;
                }
                text.push_str(if d.text.is_empty() { " " } else { &d.text });
                x += 1;
            }
        }
        if text.trim().is_empty() && !c.underline && !c.strike {
            continue;
        }
        let run = TextRun {
            len: text.len(),
            font: text_font,
            color,
            background_color: None,
            underline: c.underline.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(color),
                wavy: false,
            }),
            strikethrough: c.strike.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(color),
            }),
        };
        let force = (!c.wide).then_some(cell_w);
        let line = window.text_system().shape_line(
            text.into(),
            fitted.unwrap_or(px(typeface.size)),
            &[run],
            force,
        );
        ops.push(RowOp::Text {
            x: start,
            line: Box::new(line),
        });
    }
    ops
}

/// One frame's grid: what to draw and the caches it draws through.
struct GridPaint<'a> {
    bounds: Bounds<Pixels>,
    snapshot: &'a Snapshot,
    cell: (Pixels, Pixels),
    focused: bool,
    marked: Option<&'a str>,
    typeface: &'a Typeface,
    painted: &'a RefCell<PaintCache>,
    cursor_at: &'a Cell<CursorAt>,
}

fn paint_grid(grid: GridPaint<'_>, window: &mut Window, cx: &mut App) -> Point<Pixels> {
    let GridPaint {
        bounds,
        snapshot,
        cell: (cell_w, cell_h),
        focused,
        marked,
        typeface,
        painted,
        cursor_at,
    } = grid;
    window.paint_quad(fill(bounds, paint_color(snapshot.bg)));
    let scale = window.scale_factor();
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    // Center leftover height between top and bottom padding, like herdr.
    let spare =
        (f32::from(bounds.size.height) - 2. * PAD_Y - f32::from(cell_h) * snapshot.rows as f32)
            .max(0.);
    let origin = point(
        snap(bounds.origin.x + px(PAD_X)),
        snap(bounds.origin.y + px(PAD_Y + (spare / 2.).floor())),
    );
    let at = |x: usize, y: usize| origin + point(cell_w * x as f32, cell_h * y as f32);
    let snap_box = |b: Bounds<Pixels>| {
        let (x0, x1) = (snap(b.origin.x), snap(b.origin.x + b.size.width));
        let (y0, y1) = (snap(b.origin.y), snap(b.origin.y + b.size.height));
        Bounds::new(point(x0, y0), size(x1 - x0, y1 - y0))
    };
    let cell = (cell_w, cell_h);
    let font_key = (typeface.family.clone(), typeface.size.to_bits());

    {
        let mut cache = painted.borrow_mut();
        // A full-dirty frame or font change means none of the previous ops keep.
        if cache.cell != cell || cache.font != font_key || snapshot.all {
            cache.rows.clear();
        }
        cache.cell = cell;
        cache.font = font_key;
        cache.rows.truncate(snapshot.cells.len());
        for (y, row) in snapshot.cells.iter().enumerate() {
            let reuse = cache
                .rows
                .get(y)
                .is_some_and(|(cached, _)| Arc::ptr_eq(cached, row));
            if reuse {
                continue;
            }
            let ops = row_ops(row, cell_w, snapshot.bg, typeface, window);
            if y < cache.rows.len() {
                cache.rows[y] = (Arc::clone(row), ops);
            } else {
                cache.rows.push((Arc::clone(row), ops));
            }
        }
    }

    let cache = painted.borrow();
    for (y, (_, ops)) in cache.rows.iter().enumerate() {
        for op in ops {
            if let RowOp::Bg { from, to, color } = op {
                let b = Bounds::new(at(*from, y), size(cell_w * (*to - *from) as f32, cell_h));
                window.paint_quad(fill(snap_box(b), *color));
            }
        }
    }
    let cell_bounds = size(cell_w, cell_h);
    for (y, (_, ops)) in cache.rows.iter().enumerate() {
        for op in ops {
            match op {
                RowOp::Bg { .. } => {}
                RowOp::Sprite { x, text, color } => {
                    super::sprites::paint(
                        text,
                        Bounds::new(at(*x, y), cell_bounds),
                        *color,
                        window,
                    );
                }
                RowOp::Text { x, line } => {
                    let _ = line.paint(at(*x, y), cell_h, TextAlign::Left, None, window, cx);
                }
            }
        }
    }
    drop(cache);

    let mut cursor_bounds = None;
    if let Some(cur) = snapshot.cursor {
        let (x, y) = (cur.x as usize, cur.y as usize);
        let wide = snapshot
            .cells
            .get(y)
            .and_then(|row| row.get(x))
            .is_some_and(|c| c.wide);
        let w = cell_w * if wide { 2.0 } else { 1.0 };
        let color = paint_color(cur.color);
        let p = at(x, y);
        cursor_bounds = Some(Bounds::new(p, size(w, cell_h)));
        match (focused, cur.shape) {
            (true, CursorShape::Block) => {
                window.paint_quad(fill(snap_box(Bounds::new(p, size(w, cell_h))), color));
                if let Some(c) = snapshot
                    .cells
                    .get(y)
                    .and_then(|row| row.get(x))
                    .filter(|c| !c.text.is_empty())
                {
                    let run = TextRun {
                        len: c.text.len(),
                        font: cell_font(typeface, c.bold, c.italic),
                        color: paint_color(snapshot.bg),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        c.text.clone().into(),
                        px(typeface.size),
                        &[run],
                        None,
                    );
                    let _ = line.paint(p, cell_h, TextAlign::Left, None, window, cx);
                }
            }
            (true, CursorShape::Bar) => {
                window.paint_quad(fill(snap_box(Bounds::new(p, size(px(2.0), cell_h))), color));
            }
            (true, CursorShape::Underline) => {
                window.paint_quad(fill(
                    snap_box(Bounds::new(
                        p + point(px(0.0), cell_h - px(2.0)),
                        size(w, px(2.0)),
                    )),
                    color,
                ));
            }
            _ => {
                window.paint_quad(outline(
                    snap_box(Bounds::new(p, size(w, cell_h))),
                    color,
                    BorderStyle::Solid,
                ));
            }
        }
    } else if let Some((x, y)) = snapshot.cursor_cell {
        // Hidden cursor still hosts the IME preview.
        cursor_bounds = Some(Bounds::new(
            at(x as usize, y as usize),
            size(cell_w, cell_h),
        ));
    }

    // Composition preview at the cursor, underlined on the terminal background.
    if let (Some(bounds), Some(text)) = (cursor_bounds, marked.filter(|t| !t.is_empty())) {
        let fg = paint_color(
            snapshot
                .cursor
                .map(|c| c.color)
                .unwrap_or(Rgb(0xdd, 0xdd, 0xdd)),
        );
        let run = TextRun {
            len: text.len(),
            font: cell_font(typeface, false, false),
            color: fg,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(fg),
                wavy: false,
            }),
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(text.into(), px(typeface.size), &[run], None);
        window.paint_quad(fill(
            snap_box(Bounds::new(bounds.origin, size(line.width, cell_h))),
            paint_color(snapshot.bg),
        ));
        let _ = line.paint(bounds.origin, cell_h, TextAlign::Left, None, window, cx);
    }

    cursor_at.set(CursorAt {
        bounds: cursor_bounds,
        cell: (cell_w, cell_h),
    });
    origin
}
