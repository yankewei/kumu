//! URLs in the cells the terminal painted.
//!
//! Programs print links as plain text all the time; libghostty only knows the
//! ones wrapped in OSC 8 (see [`Vt::hyperlink_at`](super::vt::Vt::hyperlink_at)).
//! The grid the UI already holds is enough to find the text ones, so this runs
//! synchronously while the pointer moves.

use super::vt::Snapshot;

/// Schemes that mark the start of an openable URL in plain output.
const SCHEMES: [&str; 3] = ["https://", "http://", "mailto:"];

/// One row's part of a link: cell columns `from..to` on `row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub row: u16,
    pub from: u16,
    pub to: u16,
}

/// A URL found in the grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextLink {
    /// The visible runs, one per row the URL spans.
    pub spans: Vec<Span>,
    pub url: String,
}

/// The URL under `col` on `row`.
///
/// A URL that soft-wrapped continues on the next row, so the row above and the
/// rows below are read through [`Snapshot::wrap`](super::vt::Snapshot::wrap)
/// and matched as one line.
pub fn at(snapshot: &Snapshot, row: u16, col: u16) -> Option<TextLink> {
    let cells = &snapshot.cells;
    let target = usize::from(row);
    if usize::from(col) >= cells.get(target)?.len() {
        return None;
    }
    // A wrapped row continues the one above it; walk out to the whole line.
    let mut first = target;
    while first > 0 && snapshot.wrap.get(first - 1).copied().unwrap_or(false) {
        first -= 1;
    }
    let mut last = target;
    while last + 1 < cells.len() && snapshot.wrap.get(last).copied().unwrap_or(false) {
        last += 1;
    }

    // The logical line, plus the bytes each cell occupies in it.
    let mut text = String::new();
    let mut spans: Vec<(u16, u16, usize, usize)> = Vec::new();
    for (y, row_cells) in cells.iter().enumerate().take(last + 1).skip(first) {
        let mut prev_wide = false;
        for (x, cell) in row_cells.iter().enumerate() {
            // The second column of a wide character has no text of its own; it
            // shares the bytes of the character before it, so a URL written in
            // Chinese is not cut in half by a phantom space.
            if std::mem::take(&mut prev_wide) && cell.text.is_empty() {
                let (from, to) = spans.last().map_or((0, text.len()), |&(_, _, s, e)| (s, e));
                spans.push((y as u16, x as u16, from, to));
                continue;
            }
            let start = text.len();
            if cell.text.is_empty() {
                // A blank cell is a gap, not nothing: without it the words on
                // either side would run together into one bogus URL. The
                // filler that ends a soft-wrapped row for a wide character is
                // the exception — it is not a space in the text.
                let filler =
                    snapshot.wrap.get(y).copied().unwrap_or(false) && x + 1 == row_cells.len();
                if !filler {
                    text.push(' ');
                }
            } else {
                text.push_str(&cell.text);
            }
            spans.push((y as u16, x as u16, start, text.len()));
            prev_wide = cell.wide;
        }
    }
    let (_, _, hover_from, hover_to) = *spans
        .iter()
        .find(|(y, x, ..)| usize::from(*y) == target && usize::from(*x) == usize::from(col))?;

    let mut at = 0;
    while at < text.len() {
        let Some(scheme_len) = scheme_at(&text[at..]).filter(|_| boundary(&text, at)) else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let body = at + scheme_len;
        let end = end_of(&text, body);
        if end <= body {
            at = body;
            continue;
        }
        if at < hover_to && end > hover_from {
            return Some(TextLink {
                spans: runs(&spans, at, end),
                url: text[at..end].to_string(),
            });
        }
        at = end;
    }
    None
}

/// How long the URL scheme at the start of `rest` is.
fn scheme_at(rest: &str) -> Option<usize> {
    let head = rest.as_bytes();
    SCHEMES.iter().find_map(|scheme| {
        let len = scheme.len();
        (head.len() >= len && head[..len].eq_ignore_ascii_case(scheme.as_bytes())).then_some(len)
    })
}

/// Whether the scheme at `at` starts a word: `xhttps://` is not a URL.
fn boundary(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|before| !before.is_alphanumeric())
}

/// Where the URL starting at `start` ends, dropping the punctuation that
/// usually belongs to the sentence around it.
///
/// Mirrors Ghostty: no trailing `.` or `,`, no trailing `)` or `]` unless the
/// URL opened one itself (`https://en.wikipedia.org/wiki/Rust_(video_game)`).
fn end_of(text: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, ch) in text[start..].char_indices() {
        if !url_char(ch) {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    while end > start {
        let Some(last) = text[start..end].chars().next_back() else {
            break;
        };
        let url = &text[start..end];
        let trailing = match last {
            '.' | ',' | ':' => true,
            ')' => url.matches('(').count() < url.matches(')').count(),
            ']' => url.matches('[').count() < url.matches(']').count(),
            _ => false,
        };
        if !trailing {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

fn url_char(ch: char) -> bool {
    ch.is_alphanumeric() || "-._~:/?#@!$&*+,;=%()[]".contains(ch)
}

/// Whether the platform should open this URI. Terminal output is untrusted, so
/// only safe web and mail schemes are opened automatically; `file://`, script
/// schemes, and arbitrary local app handlers (which can launch binaries or
/// trigger actions via spoofed OSC 8 links) are refused.
pub fn openable(uri: &str) -> bool {
    const ALLOWED: [&str; 3] = ["http", "https", "mailto"];
    let uri = uri.trim();
    if uri.is_empty() || uri.chars().any(char::is_control) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    ALLOWED
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

/// The cell runs the URL byte range `start..end` covers.
fn runs(spans: &[(u16, u16, usize, usize)], start: usize, end: usize) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for &(row, col, from, to) in spans {
        if to <= start || from >= end {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.row == row && last.to == col => last.to = col.saturating_add(1),
            _ => out.push(Span {
                row,
                from: col,
                to: col.saturating_add(1),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Span, TextLink, at, openable};
    use crate::term::{Cell, Rgb, Snapshot};

    /// One cell per char, like a terminal row of narrow text.
    fn row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                text: ch.to_string(),
                fg: Rgb(0xdd, 0xdd, 0xdd),
                bg: None,
                bold: false,
                italic: false,
                faint: false,
                underline: false,
                strike: false,
                wide: false,
                hyperlink: false,
            })
            .collect()
    }

    /// Just the text rows (and their wrap flags) of a snapshot.
    fn grid(rows: &[&str], wrap: &[bool]) -> Snapshot {
        let mut snapshot = Snapshot::blank("t", "/");
        snapshot.cells = rows
            .iter()
            .map(|text| std::sync::Arc::new(row(text)))
            .collect();
        snapshot.wrap = wrap.to_vec();
        snapshot
    }

    fn link(url: &str, from: u16, to: u16) -> Option<TextLink> {
        Some(TextLink {
            spans: vec![Span { row: 0, from, to }],
            url: url.to_string(),
        })
    }

    #[test]
    fn a_url_under_the_pointer_is_the_url() {
        let snapshot = grid(&["see https://example.com/docs now"], &[]);
        assert_eq!(at(&snapshot, 0, 8), link("https://example.com/docs", 4, 28));
        // The cells just outside it are not the link.
        assert_eq!(at(&snapshot, 0, 3), None);
        assert_eq!(at(&snapshot, 0, 28), None);
    }

    #[test]
    fn sentence_punctuation_is_not_part_of_the_url() {
        let snapshot = grid(&["(https://example.com). next"], &[]);
        assert_eq!(at(&snapshot, 0, 3), link("https://example.com", 1, 20));
        let snapshot = grid(&["https://en.wikipedia.org/wiki/Rust_(video_game)"], &[]);
        assert_eq!(
            at(&snapshot, 0, 20),
            link("https://en.wikipedia.org/wiki/Rust_(video_game)", 0, 47)
        );
    }

    #[test]
    fn other_schemes_count_too() {
        let snapshot = grid(&["write to mailto:dev@example.com"], &[]);
        assert_eq!(at(&snapshot, 0, 12), link("mailto:dev@example.com", 9, 31));
    }

    #[test]
    fn a_url_that_soft_wrapped_spans_both_rows() {
        let snapshot = grid(&["https://example.com/", "very/long/path"], &[true]);
        assert_eq!(
            at(&snapshot, 1, 4),
            Some(TextLink {
                spans: vec![
                    Span {
                        row: 0,
                        from: 0,
                        to: 20
                    },
                    Span {
                        row: 1,
                        from: 0,
                        to: 14
                    },
                ],
                url: "https://example.com/very/long/path".to_string(),
            })
        );
        // Without the wrap flag each row is a line of its own.
        assert_eq!(
            at(
                &grid(&["https://example.com/", "very/long/path"], &[]),
                1,
                4
            ),
            None
        );
    }

    #[test]
    fn blank_cells_separate_a_url_from_its_neighbour() {
        let snapshot = grid(&["https://a.com next"], &[]);
        assert_eq!(at(&snapshot, 0, 15), None);
        assert_eq!(at(&snapshot, 0, 10), link("https://a.com", 0, 13));
    }

    #[test]
    fn only_safe_schemes_with_a_colon_are_opened() {
        for refused in [
            "javascript:alert(1)",
            " DATA:text/html,x",
            "vbscript:x",
            "file:///tmp/evil.command",
            "shortcuts://run-shortcut?name=x",
            "vscode://file/x",
            "evil.app",
            "https:",
            "/tmp/notes.md",
        ] {
            assert!(!openable(refused), "{refused}");
        }
        for allowed in [
            "https://example.com",
            "HTTP://example.com/a",
            "mailto:dev@example.com",
        ] {
            assert!(openable(allowed), "{allowed}");
        }
    }

    #[test]
    fn the_filler_at_a_wrapped_row_end_is_not_a_space() {
        // A wide character that does not fit leaves an empty filler in the last
        // column; it is not a space in the text, so a URL runs on across it.
        let mut first = row("https://example.com/");
        first.push(Cell {
            text: String::new(),
            ..row("x")[0].clone()
        });
        let mut snapshot = Snapshot::blank("t", "/");
        snapshot.cells = vec![
            std::sync::Arc::new(first),
            std::sync::Arc::new(row("very/long")),
        ];
        snapshot.wrap = vec![true];
        assert_eq!(
            at(&snapshot, 1, 2).map(|link| link.url),
            Some("https://example.com/very/long".to_string())
        );
    }

    #[test]
    fn a_wide_character_in_a_url_does_not_split_it() {
        // A wide glyph's head cell holds the text; the column after it is its
        // spacer. Both columns belong to the URL.
        let mut cells = row("https://zh.wikipedia.org/wiki/");
        cells.push(Cell {
            text: "中".into(),
            wide: true,
            ..row("x")[0].clone()
        });
        let spacer = Cell {
            text: String::new(),
            ..row("x")[0].clone()
        };
        cells.push(spacer);
        cells.extend(row("/page"));
        let mut snapshot = Snapshot::blank("t", "/");
        snapshot.cells = vec![std::sync::Arc::new(cells)];

        let last = snapshot.cells[0].len() as u16;
        let url = "https://zh.wikipedia.org/wiki/中/page";
        assert_eq!(at(&snapshot, 0, 30), link(url, 0, last));
        // The right half of the wide glyph is part of the same run.
        assert_eq!(at(&snapshot, 0, 31), link(url, 0, last));
    }
}
