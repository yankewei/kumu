use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use super::{FindBar, GridAt, TerminalEvent, TerminalView};
use crate::app::layout::PaneDirection;
use crate::term::link;
use crate::term::{
    Cell as TermCell, KeyInput, MouseAction, MouseButton as TermMouseButton, MouseInput, MouseMods,
    SelectStep,
};

/// Direction for a ⌘⌥arrow focus move.
fn pane_direction(key: &str) -> Option<PaneDirection> {
    match key {
        "left" => Some(PaneDirection::Left),
        "right" => Some(PaneDirection::Right),
        "up" => Some(PaneDirection::Up),
        "down" => Some(PaneDirection::Down),
        _ => None,
    }
}

impl TerminalView {
    pub(super) fn write_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.project.is_none() || text.is_empty() {
            return;
        }
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.terminal_write(pane, text.as_bytes().to_vec())
        });
    }

    pub(super) fn on_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // IME / character composition: the EntityInputHandler owns these.
        if event.prefer_character_input {
            return;
        }
        let mods = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        // Esc closes the find bar wherever focus is; once the user clicks the
        // grid the bar's own key handler no longer sees the event.
        if self.find.is_some() && key == "escape" && !mods.platform && !mods.control && !mods.alt {
            self.close_find(window, cx);
            cx.stop_propagation();
            return;
        }
        if mods.platform {
            // ⌘⌥arrows move focus between panes; ⌘D / ⌘⇧D split the focused
            // pane, matching the split buttons in the pane chrome. ⌘T / ⌘W
            // open and close tabs, ⌘⇧[ / ⌘⇧] walk them and ⌘1…9 picks one.
            if mods.alt && !mods.control && !mods.shift {
                if let Some(direction) = pane_direction(key) {
                    cx.emit(TerminalEvent::Focus(direction));
                    cx.stop_propagation();
                }
                return;
            }
            if !mods.alt && !mods.control {
                // Holding a mutating shortcut must not repeat: a held ⌘W
                // would otherwise turn its own close confirmation into the
                // confirming click.
                if event.is_held && matches!(key, "t" | "w" | "d") {
                    cx.stop_propagation();
                    return;
                }
                match key {
                    "t" if !mods.shift => {
                        cx.emit(TerminalEvent::NewTab);
                        cx.stop_propagation();
                        return;
                    }
                    "w" if !mods.shift => {
                        cx.emit(TerminalEvent::CloseTab);
                        cx.stop_propagation();
                        return;
                    }
                    "{" | "[" if mods.shift => {
                        cx.emit(TerminalEvent::PrevTab);
                        cx.stop_propagation();
                        return;
                    }
                    "}" | "]" if mods.shift => {
                        cx.emit(TerminalEvent::NextTab);
                        cx.stop_propagation();
                        return;
                    }
                    digit @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                        if !mods.shift =>
                    {
                        if let Ok(index) = digit.parse::<usize>() {
                            cx.emit(TerminalEvent::SelectTab(index - 1));
                            cx.stop_propagation();
                        }
                        return;
                    }
                    "d" => {
                        cx.emit(if mods.shift {
                            TerminalEvent::SplitDown
                        } else {
                            TerminalEvent::SplitRight
                        });
                        cx.stop_propagation();
                        return;
                    }
                    "f" if !mods.shift => {
                        self.open_find(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "c" if !mods.shift => {
                        if self.copy(cx) {
                            cx.stop_propagation();
                        }
                        return;
                    }
                    "v" if !mods.shift => {
                        self.paste(cx);
                        cx.stop_propagation();
                        return;
                    }
                    // Select-all is not wired yet; don't send ⌘A into the PTY.
                    _ => return,
                }
            }
            return;
        }
        if self.project.is_none() {
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
    pub(super) fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        if self.project.is_none() {
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

    pub(super) fn paste(&mut self, cx: &mut Context<Self>) {
        if self.project.is_none() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let pane = self.pane.0;
        self.workspace
            .update(cx, |workspace, _| workspace.terminal_paste(pane, text));
    }

    /// Open the find bar and focus its input. ⌘F on an open bar just brings
    /// focus back to the input.
    pub(super) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.is_none() {
            return;
        }
        if let Some(bar) = &self.find {
            let input = bar.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("在当前终端中查找"));
        let subscription = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            this.on_find_event(&input, event, cx);
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.find = Some(FindBar {
            input,
            sent: String::new(),
            _subscription: subscription,
        });
        cx.notify();
    }

    fn on_find_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(bar) = &mut self.find else {
            return;
        };
        match event {
            InputEvent::Change => {
                let value = input.read(cx).value().to_string();
                if value == bar.sent {
                    return;
                }
                bar.sent = value.clone();
                // A result frame from the previous needle must not paint or
                // report itself as this query's answer while the session
                // thread still works on the new one.
                self.snapshot.search = None;
                let pane = self.pane.0;
                self.workspace.update(cx, |workspace, _| {
                    workspace.terminal_search_set(pane, &value);
                });
                cx.notify();
            }
            // Enter walks toward older content, matching Ghostty's find bar;
            // Shift+Enter goes back toward newer output.
            InputEvent::PressEnter { shift, .. } => {
                let pane = self.pane.0;
                self.workspace.update(cx, |workspace, _| {
                    workspace.terminal_search_step(pane, !shift);
                });
                cx.notify();
            }
            _ => {}
        }
    }

    pub(super) fn step_find(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.terminal_search_step(pane, older);
        });
        // Keep the input focused so Enter keeps walking matches.
        if let Some(bar) = &self.find {
            bar.input.update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_none() {
            return;
        }
        let pane = self.pane.0;
        self.workspace.update(cx, |workspace, _| {
            workspace.terminal_search_clear(pane);
        });
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Whether the running program owns the pointer for a new gesture.
    pub(super) fn takes_mouse(&self, mods: &Modifiers) -> bool {
        self.project.is_some() && self.snapshot.mouse_tracking && !mods.shift
    }

    /// Hand a button press to the program, latching it until its Release.
    pub(super) fn press_mouse(
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
    pub(super) fn release_mouse(
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

    pub(super) fn send_mouse(
        &mut self,
        action: MouseAction,
        button: Option<gpui::MouseButton>,
        pos: Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        if self.project.is_none() {
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

    pub(super) fn start_selection(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.selecting = true;
        self.apply_select(SelectStep::Press, pos, cx);
    }

    /// End a selection drag: copy what the gesture selected. The Release step
    /// and the text both live on the session thread, so it runs off-thread.
    pub(super) fn finish_selection(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.project.is_none() {
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

    pub(super) fn apply_select(
        &mut self,
        step: SelectStep,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.project.is_none() {
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

    /// The viewport cell under a pointer position.
    fn cell_at(&self, pos: Point<Pixels>) -> Option<(u16, u16)> {
        let grid = self.grid_at.get();
        let (cw, ch) = (f32::from(grid.cell.0), f32::from(grid.cell.1));
        if cw <= 0. || ch <= 0. || !grid.bounds.contains(&pos) {
            return None;
        }
        // The padding around the cells is not a cell: clamping it to (0, 0)
        // would let a click in the margin open the top-left link.
        let x = f32::from(pos.x - grid.origin.x);
        let y = f32::from(pos.y - grid.origin.y);
        if x < 0. || y < 0. {
            return None;
        }
        let (col, row) = ((x / cw) as u16, (y / ch) as u16);
        let row_cells = self.snapshot.cells.get(usize::from(row))?;
        (usize::from(col) < row_cells.len()).then_some((col, row))
    }

    /// Highlight the link under the pointer while ⌘ is held, the macOS gesture
    /// Ghostty uses. Moving off a link, or releasing ⌘, clears it.
    pub(super) fn hover_link(
        &mut self,
        pos: Point<Pixels>,
        mods: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        let cell = mods.platform.then(|| self.cell_at(pos)).flatten();
        if cell == self.hover {
            return;
        }
        match cell {
            Some((col, row)) => self.resolve_link(col, row, cx),
            None => {
                self.hover = None;
                self.hover_row = None;
                self.hover_osc8 = false;
                self.hover_probe = self.hover_probe.wrapping_add(1);
                self.set_link(None, cx);
            }
        }
    }

    /// Resolve the link at one cell: plain URL text from the cells on this
    /// side, then an OSC 8 hyperlink from the terminal thread when the cell
    /// carries one. The probe id makes a late answer unable to replace the
    /// link under a newer cell.
    fn resolve_link(&mut self, col: u16, row: u16, cx: &mut Context<Self>) {
        self.hover = Some((col, row));
        self.hover_row = self.snapshot.cells.get(usize::from(row)).cloned();
        self.hover_osc8 = false;
        self.hover_probe = self.hover_probe.wrapping_add(1);
        let probe = self.hover_probe;
        self.set_link(
            link::at(&self.snapshot, row, col).filter(|l| link::openable(&l.url)),
            cx,
        );
        if !self.hyperlinked(row, col) {
            return;
        }
        self.probe_osc8(col, row, probe, cx);
    }

    fn probe_osc8(&self, col: u16, row: u16, probe: u64, cx: &mut Context<Self>) {
        let Some(term) = self.workspace.read(cx).terminal_handle(self.pane.0) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let hyperlink = cx
                .background_spawn(async move { term.hyperlink_at(col, row) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.hover_probe != probe {
                    return;
                }
                let Some(hyperlink) = hyperlink.filter(|hyperlink| link::openable(&hyperlink.uri))
                else {
                    if this.hover_osc8 {
                        this.hover_osc8 = false;
                        let fallback =
                            link::at(&this.snapshot, row, col).filter(|l| link::openable(&l.url));
                        this.set_link(fallback, cx);
                    }
                    return;
                };
                this.hover_osc8 = true;
                this.set_link(
                    Some(link::TextLink {
                        spans: vec![link::Span {
                            row: hyperlink.row,
                            from: hyperlink.from,
                            to: hyperlink.to,
                        }],
                        url: hyperlink.uri,
                    }),
                    cx,
                );
            });
        })
        .detach();
    }

    /// The same cell after a frame repainted its row: refresh the text link,
    /// and re-probe OSC 8 in place without clearing the current highlight so a
    /// streaming pane stays steady while a replaced link still updates its span.
    pub(super) fn refresh_link(&mut self, col: u16, row: u16, cx: &mut Context<Self>) {
        self.hover_row = self.snapshot.cells.get(usize::from(row)).cloned();
        let hyperlinked = self.hyperlinked(row, col);
        if hyperlinked {
            if self.hover_osc8 {
                let probe = self.hover_probe;
                self.probe_osc8(col, row, probe, cx);
            }
            return;
        }
        self.hover_osc8 = false;
        self.hover_probe = self.hover_probe.wrapping_add(1);
        self.set_link(
            link::at(&self.snapshot, row, col).filter(|l| link::openable(&l.url)),
            cx,
        );
    }

    /// Whether this cell is the visible text of an OSC 8 hyperlink.
    fn hyperlinked(&self, row: u16, col: u16) -> bool {
        self.snapshot
            .cells
            .get(usize::from(row))
            .and_then(|cells| cells.get(usize::from(col)))
            .is_some_and(|cell| cell.hyperlink)
    }

    fn set_link(&mut self, target: Option<link::TextLink>, cx: &mut Context<Self>) {
        if self.link == target {
            return;
        }
        self.link = target;
        cx.notify();
    }

    /// Open the link under the pointer. Called for a ⌘-click; `true` means the
    /// press was the link gesture and must not become a selection.
    ///
    /// Matches hover priority: an openable OSC 8 hyperlink takes precedence
    /// over plain-text URL scanning on the same cell.
    pub(super) fn open_link(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        if self.project.is_none() {
            return false;
        }
        let Some((col, row)) = self.cell_at(pos) else {
            return false;
        };
        let text_fallback = link::at(&self.snapshot, row, col).filter(|l| link::openable(&l.url));
        if self.hyperlinked(row, col)
            && let Some(term) = self.workspace.read(cx).terminal_handle(self.pane.0)
        {
            let cells = self.snapshot.cells.get(usize::from(row)).cloned();
            cx.spawn(async move |this, cx| {
                let hyperlink = cx
                    .background_spawn(async move { term.hyperlink_at(col, row) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // The read happens a queue turn later: if output replaced this
                    // row in the meantime, the answer is for text nobody clicked.
                    if !same_row(&cells, this.snapshot.cells.get(usize::from(row))) {
                        return;
                    }
                    if let Some(hyperlink) =
                        hyperlink.filter(|hyperlink| link::openable(&hyperlink.uri))
                    {
                        cx.open_url(&hyperlink.uri);
                    } else if let Some(link) = text_fallback {
                        cx.open_url(&link.url);
                    }
                });
            })
            .detach();
            return true;
        }
        if let Some(link) = text_fallback {
            cx.open_url(&link.url);
            return true;
        }
        false
    }

    pub(super) fn on_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        if self.project.is_none() {
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

/// Whether a captured row is still the row at that position (the same [`Arc`],
/// so libghostty did not repaint it).
fn same_row(captured: &Option<Arc<Vec<TermCell>>>, now: Option<&Arc<Vec<TermCell>>>) -> bool {
    match (captured, now) {
        (Some(captured), Some(now)) => Arc::ptr_eq(captured, now),
        (None, None) => true,
        _ => false,
    }
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
        let start = range.start.min(units.len());
        let end = range.end.clamp(start, units.len());
        let range = start..end;
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
