//! URLs in the cells the terminal painted.
//!
//! Programs print links as plain text all the time; libghostty only knows the
//! ones wrapped in OSC 8 (see [`Vt::hyperlink_at`](super::vt::Vt::hyperlink_at)).
//! The grid the UI already holds is enough to find the text ones, so this runs
//! synchronously while the pointer moves.

use std::path::{Path, PathBuf};

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

/// A local file reference (`path[:line[:col]]`) found in terminal output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTarget {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

/// Either a web/mailto URL or a verified local source file reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    File(FileTarget),
}

/// A hoverable / clickable terminal link (URL or local file:line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLink {
    pub spans: Vec<Span>,
    pub target: LinkTarget,
}

/// Reconstruct the soft-wrapped logical line around `(row, col)` along with
/// byte ranges per cell and the hovered cell's byte span.
#[allow(clippy::type_complexity)]
fn logical_line(
    snapshot: &Snapshot,
    row: u16,
    col: u16,
) -> Option<(String, Vec<(u16, u16, usize, usize)>, usize, usize)> {
    let cells = &snapshot.cells;
    let target = usize::from(row);
    if usize::from(col) >= cells.get(target)?.len() {
        return None;
    }
    let mut first = target;
    while first > 0 && snapshot.wrap.get(first - 1).copied().unwrap_or(false) {
        first -= 1;
    }
    let mut last = target;
    while last + 1 < cells.len() && snapshot.wrap.get(last).copied().unwrap_or(false) {
        last += 1;
    }

    let mut text = String::new();
    let mut spans: Vec<(u16, u16, usize, usize)> = Vec::new();
    for (y, row_cells) in cells.iter().enumerate().take(last + 1).skip(first) {
        let mut prev_wide = false;
        for (x, cell) in row_cells.iter().enumerate() {
            if std::mem::take(&mut prev_wide) && cell.text.is_empty() {
                let (from, to) = spans.last().map_or((0, text.len()), |&(_, _, s, e)| (s, e));
                spans.push((y as u16, x as u16, from, to));
                continue;
            }
            let start = text.len();
            if cell.text.is_empty() {
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
    Some((text, spans, hover_from, hover_to))
}

/// The URL under `col` on `row`.
///
/// A URL that soft-wrapped continues on the next row, so the row above and the
/// rows below are read through [`Snapshot::wrap`](super::vt::Snapshot::wrap)
/// and matched as one line.
pub fn at(snapshot: &Snapshot, row: u16, col: u16) -> Option<TextLink> {
    let (text, spans, hover_from, hover_to) = logical_line(snapshot, row, col)?;

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

/// Resolve either an openable URL or an existing local `file[:line[:col]]`
/// reference under `(row, col)`.
pub fn resolve_at(snapshot: &Snapshot, row: u16, col: u16) -> Option<ResolvedLink> {
    if let Some(url) = at(snapshot, row, col).filter(|l| openable(&l.url)) {
        return Some(ResolvedLink {
            spans: url.spans,
            target: LinkTarget::Url(url.url),
        });
    }
    file_at(snapshot, row, col)
}

/// Resolve a plain-text local file reference (`src/foo.rs:42:10`,
/// `./Cargo.toml`, `/abs/path.rs(12,3)`, `~/notes.md`) under `(row, col)`.
///
/// Only returns a match when the resolved path exists on disk as a regular,
/// non-bundle file, preventing false positives on prose and blocking arbitrary
/// app/script execution.
pub fn file_at(snapshot: &Snapshot, row: u16, col: u16) -> Option<ResolvedLink> {
    let (text, spans, hover_from, hover_to) = logical_line(snapshot, row, col)?;

    // Expand to the surrounding non-whitespace token covering the hovered cell.
    let mut tok_start = hover_from;
    while tok_start > 0 {
        let ch = text[..tok_start].chars().next_back()?;
        if ch.is_whitespace() {
            break;
        }
        tok_start -= ch.len_utf8();
    }
    let mut tok_end = hover_to.min(text.len());
    while tok_end < text.len() {
        let ch = text[tok_end..].chars().next()?;
        if ch.is_whitespace() {
            break;
        }
        tok_end += ch.len_utf8();
    }
    if tok_start >= tok_end {
        return None;
    }

    // Trim surrounding quotes and sentence/bracket wrappers while keeping
    // balanced `(line,col)` suffixes intact.
    let mut start = tok_start;
    let mut end = tok_end;
    while start < end {
        let ch = text[start..end].chars().next()?;
        if matches!(ch, '\'' | '"' | '`' | '(' | '[' | '{' | '<') {
            start += ch.len_utf8();
        } else {
            break;
        }
    }
    while end > start {
        let slice = &text[start..end];
        let ch = slice.chars().next_back()?;
        let trim = match ch {
            '\'' | '"' | '`' | ',' | ';' | ':' | '.' | '>' | '}' | ']' => true,
            ')' => slice.matches('(').count() < slice.matches(')').count(),
            _ => false,
        };
        if trim {
            end -= ch.len_utf8();
        } else {
            break;
        }
    }
    if start >= end || start >= hover_to || end <= hover_from {
        return None;
    }

    let token = &text[start..end];
    if token.contains("://") {
        return None;
    }
    let (raw_path, line, col_num) = parse_file_location(token)?;
    if raw_path.is_empty()
        || raw_path == "."
        || raw_path == ".."
        || (!raw_path.contains('/') && !raw_path.contains('.'))
    {
        return None;
    }

    let resolved = resolve_existing_file(raw_path, &snapshot.cwd)?;
    Some(ResolvedLink {
        spans: runs(&spans, start, end),
        target: LinkTarget::File(FileTarget {
            path: resolved,
            line,
            col: col_num,
        }),
    })
}

fn parse_file_location(token: &str) -> Option<(&str, Option<u32>, Option<u32>)> {
    // Form 1: `path/to/file.rs(42,10)` or `path/to/file.rs(42)`
    if let Some(open) = token.rfind('(')
        && token.ends_with(')')
        && open > 0
    {
        let inside = &token[open + 1..token.len() - 1];
        if let Some((l, c)) = inside.split_once(',') {
            if let (Ok(line), Ok(col)) = (l.parse::<u32>(), c.parse::<u32>())
                && line > 0
            {
                return Some((&token[..open], Some(line), (col > 0).then_some(col)));
            }
        } else if let Ok(line) = inside.parse::<u32>()
            && line > 0
        {
            return Some((&token[..open], Some(line), None));
        }
    }

    // Form 2: `path/to/file.rs:42:10` or `path/to/file.rs:42`
    if let Some((head, tail)) = token.rsplit_once(':')
        && let Ok(last_num) = tail.parse::<u32>()
        && last_num > 0
    {
        if let Some((path, line_str)) = head.rsplit_once(':')
            && let Ok(line) = line_str.parse::<u32>()
            && line > 0
        {
            return Some((path, Some(line), Some(last_num)));
        }
        return Some((head, Some(last_num), None));
    }

    Some((token, None, None))
}

fn resolve_existing_file(raw: &str, cwd: &str) -> Option<PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            candidates.push(PathBuf::from(home).join(rest));
        }
    } else if Path::new(raw).is_absolute() {
        candidates.push(PathBuf::from(raw));
    } else if !cwd.is_empty() {
        let base = Path::new(cwd);
        candidates.push(base.join(raw));
        // Git unified diff prefixes (`a/src/foo.rs`, `b/src/foo.rs`).
        if let Some(stripped) = raw.strip_prefix("a/").or_else(|| raw.strip_prefix("b/")) {
            candidates.push(base.join(stripped));
        }
    }

    candidates
        .into_iter()
        .find(|path| is_safe_openable_file(path))
}

fn is_safe_openable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && matches!(
            ext.to_ascii_lowercase().as_str(),
            "app" | "command" | "tool" | "pkg" | "dmg" | "workflow" | "scpt"
        )
    {
        return false;
    }
    true
}

/// Open a verified local file in the user's code editor (jumping to `line:col`
/// when supported), falling back to macOS `open -t` so files are never
/// executed as scripts or apps.
pub fn open_file_in_editor(target: &FileTarget) {
    let path_str = target.path.to_string_lossy();
    let loc = match (target.line, target.col) {
        (Some(line), Some(col)) => format!("{path_str}:{line}:{col}"),
        (Some(line), None) => format!("{path_str}:{line}"),
        _ => path_str.to_string(),
    };

    if let Some((bin, kind)) = detect_editor() {
        let mut cmd = std::process::Command::new(&bin);
        match kind {
            EditorKind::GotoFlag => {
                cmd.arg("--goto").arg(&loc);
            }
            EditorKind::ColonLocation => {
                cmd.arg(&loc);
            }
            EditorKind::PlainPath => {
                cmd.arg(&target.path);
            }
        }
        if cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }

    let _ = std::process::Command::new("/usr/bin/open")
        .arg("-t")
        .arg("--")
        .arg(&target.path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[derive(Clone, Copy)]
enum EditorKind {
    GotoFlag,
    ColonLocation,
    PlainPath,
}

fn editor_kind_for(bin: &str) -> EditorKind {
    let name = Path::new(bin)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(bin)
        .to_ascii_lowercase();
    match name.as_str() {
        "code" | "code-insiders" | "cursor" | "windsurf" => EditorKind::GotoFlag,
        "zed" | "cli" | "subl" => EditorKind::ColonLocation,
        _ => EditorKind::PlainPath,
    }
}

fn detect_editor() -> Option<(String, EditorKind)> {
    for env_key in ["KUMU_EDITOR", "VISUAL", "EDITOR"] {
        if let Ok(raw) = std::env::var(env_key) {
            let trimmed = raw.trim();
            if let Some(bin) = trimmed.split_whitespace().next()
                && !bin.is_empty()
            {
                let name = Path::new(bin)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(bin);
                // Skip terminal-only editors (vim/nvim/nano) since there is no
                // controlling TTY when launching from a GUI click.
                if !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "vi" | "vim" | "nvim" | "nano" | "less" | "more"
                ) && let Some(resolved) = find_executable(bin)
                {
                    let kind = editor_kind_for(&resolved);
                    return Some((resolved, kind));
                }
            }
        }
    }

    let candidates = [
        ("zed", EditorKind::ColonLocation),
        (
            "/Applications/Zed.app/Contents/MacOS/cli",
            EditorKind::ColonLocation,
        ),
        ("cursor", EditorKind::GotoFlag),
        (
            "/Applications/Cursor.app/Contents/Resources/app/bin/cursor",
            EditorKind::GotoFlag,
        ),
        ("code", EditorKind::GotoFlag),
        (
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
            EditorKind::GotoFlag,
        ),
    ];
    for (bin, kind) in candidates {
        if let Some(resolved) = find_executable(bin) {
            return Some((resolved, kind));
        }
    }
    None
}

fn find_executable(cmd: &str) -> Option<String> {
    let is_exec = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| {
            if !m.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    };
    if cmd.contains(std::path::MAIN_SEPARATOR) {
        return is_exec(Path::new(cmd)).then(|| cmd.to_string());
    }
    for path_var in [
        std::env::var_os("PATH"),
        crate::term::shell_env::cached_path(),
    ]
    .into_iter()
    .flatten()
    {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(cmd);
            if is_exec(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let candidate = Path::new(dir).join(cmd);
        if is_exec(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
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

    #[test]
    fn file_at_resolves_existing_paths_with_line_and_column() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-link-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let evil = dir.join("run.command");
        std::fs::write(&evil, "#!/bin/sh\n").unwrap();

        let cwd = dir.to_string_lossy().into_owned();
        let mut snap = grid(
            &[
                "error at src/main.rs:42:10 and (b/src/main.rs:7), src/main.rs(12,3)",
                "missing src/nope.rs:9 and run.command",
            ],
            &[],
        );
        snap.cwd = cwd;

        let hit = super::file_at(&snap, 0, 12).expect("src/main.rs:42:10");
        assert_eq!(
            hit.target,
            super::LinkTarget::File(super::FileTarget {
                path: file.clone(),
                line: Some(42),
                col: Some(10),
            })
        );
        let diff_hit = super::file_at(&snap, 0, 35).expect("b/src/main.rs:7");
        assert_eq!(
            diff_hit.target,
            super::LinkTarget::File(super::FileTarget {
                path: file.clone(),
                line: Some(7),
                col: None,
            })
        );
        let paren_hit = super::file_at(&snap, 0, 55).expect("src/main.rs(12,3)");
        assert_eq!(
            paren_hit.target,
            super::LinkTarget::File(super::FileTarget {
                path: file,
                line: Some(12),
                col: Some(3),
            })
        );
        // Non-existent files and macOS executable command scripts are ignored.
        assert_eq!(super::file_at(&snap, 1, 12), None);
        assert_eq!(super::file_at(&snap, 1, 30), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
