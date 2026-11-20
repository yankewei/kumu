use std::ffi::c_void;

use ghostty_sys::*;

use super::{SearchHit, SearchView, SelectStep, Vt, ok};

impl Vt {
    /// Whether the terminal currently owns a selection.
    pub fn has_selection(&self) -> bool {
        let mut selection = GhosttySelection::default();
        unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_SELECTION,
                &mut selection as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
        }
    }

    /// Clear the selection, reporting whether there was one. Callers draw only
    /// on a change, so typing in a terminal with nothing selected must not
    /// produce a new snapshot.
    pub fn clear_selection(&mut self) -> bool {
        if !self.has_selection() {
            return false;
        }
        unsafe {
            ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null());
        }
        true
    }

    /// Selected plain text, if any.
    pub fn selection_text(&self) -> Option<String> {
        unsafe {
            let mut selection = GhosttySelection::default();
            if ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_SELECTION,
                &mut selection as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                return None;
            }
            let mut formatter = std::ptr::null_mut();
            if ghostty_formatter_terminal_new(std::ptr::null(), &mut formatter, self.term)
                != GHOSTTY_SUCCESS
            {
                return None;
            }
            let emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
            let unwrap = true;
            let trim = true;
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_EMIT,
                &emit as *const _ as *const c_void,
            );
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_UNWRAP,
                &unwrap as *const _ as *const c_void,
            );
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_TRIM,
                &trim as *const _ as *const c_void,
            );
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_SELECTION,
                &selection as *const _ as *const c_void,
            );
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0usize;
            let status =
                ghostty_formatter_format_alloc(formatter, std::ptr::null(), &mut ptr, &mut len);
            let text = if status == GHOSTTY_SUCCESS && !ptr.is_null() && len > 0 {
                let text =
                    String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned();
                ghostty_free(std::ptr::null(), ptr, len);
                text
            } else {
                String::new()
            };
            ghostty_formatter_free(formatter);
            (!text.is_empty()).then_some(text)
        }
    }

    /// Set the find bar's needle and jump to the newest match. An empty needle
    /// closes the search.
    pub fn search_set(&mut self, needle: &str) {
        if needle.is_empty() {
            self.search_clear();
            return;
        }
        let Some(search) = self.ensure_search() else {
            return;
        };
        let value = GhosttyString {
            ptr: needle.as_ptr(),
            len: needle.len(),
        };
        let status = unsafe {
            ghostty_search_set(
                search,
                GHOSTTY_SEARCH_OPT_NEEDLE,
                &value as *const _ as *const c_void,
            )
        };
        if status != GHOSTTY_SUCCESS {
            return;
        }
        self.needle = needle.to_string();
        unsafe {
            let _ = ghostty_search_run(search);
            // Typing a query lands on the newest match, like Ghostty's find
            // bar; Enter from there walks toward older content.
            let _ = ghostty_search_select_next(search);
        }
    }

    /// Move the selected match toward older (`older`) or newer content.
    pub fn search_step(&mut self, older: bool) -> bool {
        if self.needle.is_empty() {
            return false;
        }
        let Some(search) = self.search else {
            return false;
        };
        let status = unsafe {
            if older {
                ghostty_search_select_next(search)
            } else {
                ghostty_search_select_prev(search)
            }
        };
        status == GHOSTTY_SUCCESS
    }

    pub fn search_clear(&mut self) -> bool {
        if self.needle.is_empty() {
            return false;
        }
        self.needle.clear();
        if let Some(search) = self.search {
            unsafe {
                ghostty_search_set(search, GHOSTTY_SEARCH_OPT_NEEDLE, std::ptr::null());
            }
        }
        true
    }

    /// Let the search see terminal changes since the last frame. No-op while
    /// no needle is set, so ordinary output costs nothing.
    pub fn search_feed(&mut self) {
        if self.needle.is_empty() {
            return;
        }
        if let Some(search) = self.search {
            unsafe {
                let _ = ghostty_search_feed(search);
            }
        }
    }

    fn ensure_search(&mut self) -> Option<GhosttySearch> {
        if let Some(search) = self.search {
            return Some(search);
        }
        let mut search = std::ptr::null_mut();
        unsafe {
            ok(
                ghostty_search_new(std::ptr::null(), &mut search, self.term),
                "search_new",
            )
        };
        // A successful call always yields a handle. Refuse to cache a null one
        // so a later query can try again instead of silently searching nothing.
        if search.is_null() {
            return None;
        }
        self.search = Some(search);
        Some(search)
    }

    /// Read the current search state and translate visible matches into
    /// viewport rows and columns for painting.
    pub(super) fn search_view(&self) -> Option<SearchView> {
        if self.needle.is_empty() {
            return None;
        }
        let search = self.search?;
        unsafe {
            let mut total: usize = 0;
            if ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_TOTAL_MATCHES,
                &mut total as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                return None;
            }
            let mut index: usize = 0;
            let selected = (ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_SELECTED_INDEX,
                &mut index as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS)
                .then_some(index);
            let mut selected_match = GhosttySelection::default();
            let has_selected = ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_SELECTED_MATCH,
                &mut selected_match as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS;

            // Query the required capacity first; the viewport list is a page
            // or two of matches, so one allocation is enough.
            let mut needed = GhosttySelectionBuffer {
                ptr: std::ptr::null_mut(),
                cap: 0,
                len: 0,
            };
            let status = ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
                &mut needed as *mut _ as *mut c_void,
            );
            if needed.len == 0 {
                return Some(SearchView {
                    needle: self.needle.clone(),
                    total,
                    selected,
                    hits: Vec::new(),
                });
            }
            if status != GHOSTTY_SUCCESS && status != GHOSTTY_OUT_OF_SPACE {
                return Some(SearchView {
                    needle: self.needle.clone(),
                    total,
                    selected,
                    hits: Vec::new(),
                });
            }
            let mut matches = vec![GhosttySelection::default(); needed.len];
            let mut buffer = GhosttySelectionBuffer {
                ptr: matches.as_mut_ptr(),
                cap: matches.len(),
                len: 0,
            };
            if ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
                &mut buffer as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                return Some(SearchView {
                    needle: self.needle.clone(),
                    total,
                    selected,
                    hits: Vec::new(),
                });
            }
            matches.truncate(buffer.len.min(matches.len()));

            let rows = self.rows;
            let cols = self.cols;
            let mut hits = Vec::new();
            for selection in &matches {
                let mut ordered = GhosttySelection::default();
                if ghostty_terminal_selection_ordered(
                    self.term,
                    selection,
                    GHOSTTY_SELECTION_ORDER_FORWARD,
                    &mut ordered,
                ) != GHOSTTY_SUCCESS
                {
                    continue;
                }
                let (mut start, mut end) = (
                    GhosttyPointCoordinate::default(),
                    GhosttyPointCoordinate::default(),
                );
                if ghostty_terminal_point_from_grid_ref(
                    self.term,
                    &ordered.start,
                    GHOSTTY_POINT_TAG_VIEWPORT,
                    &mut start,
                ) != GHOSTTY_SUCCESS
                    || ghostty_terminal_point_from_grid_ref(
                        self.term,
                        &ordered.end,
                        GHOSTTY_POINT_TAG_VIEWPORT,
                        &mut end,
                    ) != GHOSTTY_SUCCESS
                {
                    continue;
                }
                if start.y >= rows as u32 || end.y >= rows as u32 {
                    continue;
                }
                let mut equal = false;
                let selected = has_selected
                    && ghostty_terminal_selection_equal(
                        self.term,
                        selection,
                        &selected_match,
                        &mut equal,
                    ) == GHOSTTY_SUCCESS
                    && equal;
                let max_col = cols.saturating_sub(1);
                let last = end.x.min(max_col);
                let first = start.x.min(max_col);
                if start.y == end.y {
                    let first = first.min(last);
                    hits.push(SearchHit {
                        row: start.y as u16,
                        from: first,
                        to: last.saturating_add(1),
                        selected,
                    });
                } else {
                    hits.push(SearchHit {
                        row: start.y as u16,
                        from: first,
                        to: cols,
                        selected,
                    });
                    for row in start.y + 1..end.y {
                        hits.push(SearchHit {
                            row: row as u16,
                            from: 0,
                            to: cols,
                            selected,
                        });
                    }
                    hits.push(SearchHit {
                        row: end.y as u16,
                        from: 0,
                        to: last.saturating_add(1),
                        selected,
                    });
                }
            }
            Some(SearchView {
                needle: self.needle.clone(),
                total,
                selected,
                hits,
            })
        }
    }

    /// One mouse step of a selection gesture. Positions are in grid-local pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        &mut self,
        step: SelectStep,
        col: u16,
        row: u16,
        x: f32,
        y: f32,
        cell_w: f32,
        height: f32,
    ) {
        unsafe {
            let mut grid = GhosttyGridRef::default();
            let mut point = GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_VIEWPORT,
                ..Default::default()
            };
            point.value.coordinate = GhosttyPointCoordinate {
                x: col.min(self.cols.saturating_sub(1)),
                y: u32::from(row.min(self.rows.saturating_sub(1))),
            };
            if ghostty_terminal_grid_ref(self.term, point, &mut grid) != GHOSTTY_SUCCESS {
                return;
            }
            let pos = GhosttySurfacePosition {
                x: f64::from(x),
                y: f64::from(y),
            };
            let event = match step {
                SelectStep::Press => self.press,
                SelectStep::Drag => self.drag,
                SelectStep::Release => self.release,
            };
            ghostty_selection_gesture_event_set(
                event,
                GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                &grid as *const _ as *const c_void,
            );
            if !matches!(step, SelectStep::Release) {
                ghostty_selection_gesture_event_set(
                    event,
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                    &pos as *const _ as *const c_void,
                );
            }
            match step {
                SelectStep::Press => {
                    let now = self.epoch.elapsed().as_nanos() as u64;
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                        &now as *const _ as *const c_void,
                    );
                    let interval = 500_000_000u64;
                    let distance = 4.0f64;
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                        &interval as *const _ as *const c_void,
                    );
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                        &distance as *const _ as *const c_void,
                    );
                }
                SelectStep::Drag => {
                    let geometry = GhosttySelectionGestureGeometry {
                        columns: u32::from(self.cols),
                        cell_width: cell_w.max(1.0).round() as u32,
                        padding_left: 0,
                        screen_height: height.max(1.0).round() as u32,
                    };
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                        &geometry as *const _ as *const c_void,
                    );
                }
                SelectStep::Release => {}
            }
            let mut selection = GhosttySelection::default();
            match ghostty_selection_gesture_event(self.gesture, self.term, event, &mut selection) {
                GHOSTTY_SUCCESS => {
                    ghostty_terminal_set(
                        self.term,
                        GHOSTTY_TERMINAL_OPT_SELECTION,
                        &selection as *const _ as *const c_void,
                    );
                }
                _ if matches!(step, SelectStep::Press) => {
                    ghostty_terminal_set(
                        self.term,
                        GHOSTTY_TERMINAL_OPT_SELECTION,
                        std::ptr::null(),
                    );
                }
                _ => {}
            }
        }
    }
}
