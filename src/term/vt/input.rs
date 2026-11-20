use std::ffi::c_void;

use ghostty_sys::*;

use super::{KeyInput, MouseAction, MouseButton, MouseInput, Vt};

/// DEC private mode 2004. The C header spells it `GHOSTTY_MODE_BRACKETED_PASTE`,
/// but that macro is built by an inline helper bindgen does not emit.
const MODE_BRACKETED_PASTE: GhosttyMode = 2004;

impl Vt {
    /// Encode a mouse event for a program that tracks the mouse.
    pub fn encode_mouse(&mut self, input: MouseInput) -> Vec<u8> {
        let action = match input.action {
            MouseAction::Press => GHOSTTY_MOUSE_ACTION_PRESS,
            MouseAction::Release => GHOSTTY_MOUSE_ACTION_RELEASE,
            MouseAction::Motion => GHOSTTY_MOUSE_ACTION_MOTION,
        };
        let button = input.button.map(|b| match b {
            MouseButton::Left => GHOSTTY_MOUSE_BUTTON_LEFT,
            MouseButton::Right => GHOSTTY_MOUSE_BUTTON_RIGHT,
            MouseButton::Middle => GHOSTTY_MOUSE_BUTTON_MIDDLE,
            MouseButton::WheelUp => GHOSTTY_MOUSE_BUTTON_FOUR,
            MouseButton::WheelDown => GHOSTTY_MOUSE_BUTTON_FIVE,
        });
        let (cw, ch) = (input.cell_w.round().max(1.0), input.cell_h.round().max(1.0));
        let (x, y) = (
            input.x / input.cell_w.max(1.0) * cw,
            input.y / input.cell_h.max(1.0) * ch,
        );
        unsafe {
            ghostty_mouse_encoder_set_from_terminal(self.mouse, self.term);
            let size = GhosttyMouseEncoderSize {
                size: std::mem::size_of::<GhosttyMouseEncoderSize>(),
                screen_width: u32::from(self.cols) * cw as u32,
                screen_height: u32::from(self.rows) * ch as u32,
                cell_width: cw as u32,
                cell_height: ch as u32,
                ..Default::default()
            };
            ghostty_mouse_encoder_set(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &size as *const _ as *const c_void,
            );
            ghostty_mouse_encoder_set(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &input.any_pressed as *const _ as *const c_void,
            );
            let event = self.mouse_event;
            ghostty_mouse_event_set_action(event, action);
            match button {
                Some(b) => {
                    ghostty_mouse_event_set_button(event, b);
                }
                None => {
                    ghostty_mouse_event_clear_button(event);
                }
            }
            let mut mods: GhosttyMods = 0;
            if input.mods.shift {
                mods |= GHOSTTY_MODS_SHIFT as GhosttyMods;
            }
            if input.mods.control {
                mods |= GHOSTTY_MODS_CTRL as GhosttyMods;
            }
            if input.mods.alt {
                mods |= GHOSTTY_MODS_ALT as GhosttyMods;
            }
            ghostty_mouse_event_set_mods(event, mods);
            ghostty_mouse_event_set_position(event, GhosttyMousePosition { x, y });
            let mut buf = [0u8; 64];
            let mut len = 0usize;
            let status = ghostty_mouse_encoder_encode(
                self.mouse,
                event,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                &mut len,
            );
            if status != GHOSTTY_SUCCESS || len == 0 {
                return Vec::new();
            }
            if action == GHOSTTY_MOUSE_ACTION_MOTION && self.last_mouse_report == buf[..len] {
                return Vec::new();
            }
            self.last_mouse_report.clear();
            self.last_mouse_report.extend_from_slice(&buf[..len]);
            buf[..len].to_vec()
        }
    }

    /// Arrow-key scroll for alternate-screen programs that don't take the mouse.
    pub fn encode_scroll_keys(&mut self, up: bool, n: usize) -> Vec<u8> {
        let key = if up {
            GHOSTTY_KEY_ARROW_UP
        } else {
            GHOSTTY_KEY_ARROW_DOWN
        };
        let mut out = Vec::new();
        for _ in 0..n {
            out.extend(self.encode_key(key, 0, 0, None, None));
        }
        out
    }

    /// Whether the program asked for bracketed paste (DEC private mode 2004).
    pub fn bracketed_paste(&self) -> bool {
        let mut mode = GhosttyTerminalModeConfig {
            mode: MODE_BRACKETED_PASTE,
            value: false,
        };
        let ok = unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_MODE,
                &mut mode as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
        };
        ok && mode.value
    }

    /// Pasting text for the PTY: control bytes stripped, wrapped in bracketed
    /// paste markers when the program asked for them.
    ///
    /// Without the markers a multi-line paste is indistinguishable from typing,
    /// so every newline is executed — which is why programs enable mode 2004.
    pub fn encode_paste(&self, text: &[u8]) -> Vec<u8> {
        let bracketed = self.bracketed_paste();
        let mut data = text.to_vec();
        // Bracketed paste adds `ESC[200~` and `ESC[201~`; everything else in
        // the encoding keeps the input length.
        let mut buf = vec![0u8; data.len() + 12];
        let mut written = 0usize;
        let mut status = unsafe {
            ghostty_paste_encode(
                data.as_mut_ptr() as *mut ::std::os::raw::c_char,
                data.len(),
                bracketed,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };
        if status == GHOSTTY_OUT_OF_SPACE {
            buf = vec![0u8; written];
            status = unsafe {
                ghostty_paste_encode(
                    data.as_mut_ptr() as *mut ::std::os::raw::c_char,
                    data.len(),
                    bracketed,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut written,
                )
            };
        }
        if status != GHOSTTY_SUCCESS || written == 0 {
            return Vec::new();
        }
        buf.truncate(written);
        buf
    }

    pub fn encode_input(&mut self, input: &KeyInput) -> Vec<u8> {
        let Some((key, shifted, unshifted)) = map_key(&input.key) else {
            return Vec::new();
        };
        let mut mods: GhosttyMods = 0;
        if input.shift || shifted {
            mods |= GHOSTTY_MODS_SHIFT as GhosttyMods;
        }
        if input.control {
            mods |= GHOSTTY_MODS_CTRL as GhosttyMods;
        }
        if input.alt {
            mods |= GHOSTTY_MODS_ALT as GhosttyMods;
        }
        let text = if input.control {
            None
        } else if input.key == "space" {
            Some(" ".to_string())
        } else {
            input.key_char.clone()
        };
        let printable = text
            .as_deref()
            .is_some_and(|text| !text.is_empty() && !text.chars().any(|c| c.is_control()));
        let consumed = if printable {
            mods & GHOSTTY_MODS_SHIFT as GhosttyMods
        } else {
            0
        };
        self.encode_key(
            key,
            mods,
            consumed,
            text.filter(|_| printable).as_deref(),
            unshifted,
        )
    }

    fn encode_key(
        &mut self,
        key: GhosttyKey,
        mods: GhosttyMods,
        consumed: GhosttyMods,
        text: Option<&str>,
        unshifted: Option<char>,
    ) -> Vec<u8> {
        unsafe {
            ghostty_key_encoder_set_from_terminal(self.keys, self.term);
            let event = self.key_event;
            ghostty_key_event_set_action(event, GHOSTTY_KEY_ACTION_PRESS);
            ghostty_key_event_set_key(event, key);
            ghostty_key_event_set_mods(event, mods);
            ghostty_key_event_set_consumed_mods(event, consumed);
            let utf8 = match text {
                Some(text) => GhosttyString {
                    ptr: text.as_ptr().cast(),
                    len: text.len(),
                },
                None => GhosttyString {
                    ptr: std::ptr::null(),
                    len: 0,
                },
            };
            ghostty_key_event_set_utf8(event, utf8);
            ghostty_key_event_set_unshifted_codepoint(event, unshifted.map_or(0, |c| c as u32));
            let mut buf = [0u8; 128];
            let mut len = 0usize;
            let status = ghostty_key_encoder_encode(
                self.keys,
                event,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                &mut len,
            );
            if status != GHOSTTY_SUCCESS {
                return Vec::new();
            }
            buf[..len].to_vec()
        }
    }
}

fn map_key(key: &str) -> Option<(GhosttyKey, bool, Option<char>)> {
    let named = match key {
        "enter" => GHOSTTY_KEY_ENTER,
        "tab" => GHOSTTY_KEY_TAB,
        "escape" => GHOSTTY_KEY_ESCAPE,
        "backspace" => GHOSTTY_KEY_BACKSPACE,
        "space" => return Some((GHOSTTY_KEY_SPACE, false, Some(' '))),
        "delete" => GHOSTTY_KEY_DELETE,
        "insert" => GHOSTTY_KEY_INSERT,
        "home" => GHOSTTY_KEY_HOME,
        "end" => GHOSTTY_KEY_END,
        "pageup" => GHOSTTY_KEY_PAGE_UP,
        "pagedown" => GHOSTTY_KEY_PAGE_DOWN,
        "up" => GHOSTTY_KEY_ARROW_UP,
        "down" => GHOSTTY_KEY_ARROW_DOWN,
        "left" => GHOSTTY_KEY_ARROW_LEFT,
        "right" => GHOSTTY_KEY_ARROW_RIGHT,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    };
    if named != GHOSTTY_KEY_UNIDENTIFIED {
        return Some((named, false, None));
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=12).contains(&n)
    {
        return Some((GHOSTTY_KEY_F1 + (n - 1) as GhosttyKey, false, None));
    }
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    const SHIFTED: &[(char, char)] = &[
        ('~', '`'),
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
        ('_', '-'),
        ('+', '='),
        ('{', '['),
        ('}', ']'),
        ('|', '\\'),
        (':', ';'),
        ('"', '\''),
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
    ];
    let (base, shifted) = match SHIFTED.iter().find(|(shifted, _)| *shifted == c) {
        Some((_, base)) => (*base, true),
        None => (c, false),
    };
    let code = match base {
        'a'..='z' => GHOSTTY_KEY_A + (base as u8 - b'a') as GhosttyKey,
        '0'..='9' => GHOSTTY_KEY_DIGIT_0 + (base as u8 - b'0') as GhosttyKey,
        '`' => GHOSTTY_KEY_BACKQUOTE,
        '\\' => GHOSTTY_KEY_BACKSLASH,
        '[' => GHOSTTY_KEY_BRACKET_LEFT,
        ']' => GHOSTTY_KEY_BRACKET_RIGHT,
        ',' => GHOSTTY_KEY_COMMA,
        '=' => GHOSTTY_KEY_EQUAL,
        '-' => GHOSTTY_KEY_MINUS,
        '.' => GHOSTTY_KEY_PERIOD,
        '\'' => GHOSTTY_KEY_QUOTE,
        ';' => GHOSTTY_KEY_SEMICOLON,
        '/' => GHOSTTY_KEY_SLASH,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    };
    (code != GHOSTTY_KEY_UNIDENTIFIED).then_some((code, shifted, Some(base)))
}
