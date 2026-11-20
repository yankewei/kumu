use super::*;

fn cell(snapshot: &Snapshot, x: usize, y: usize) -> &Cell {
    &snapshot.cells[y][x]
}

#[test]
fn terminal_notifications_parse_across_pty_reads() {
    let mut vt = Vt::new(80, 24);
    vt.write(b"\x1b]9;");
    assert_eq!(vt.take_notification(), None);
    vt.write("任务完成".as_bytes());
    // Ghostty dispatches OSC on ESC, before consuming ST's final slash.
    vt.write(b"\x1b");
    assert_eq!(
        vt.take_notification(),
        Some(TermNotification {
            title: String::new(),
            body: "任务完成".into(),
        })
    );
    vt.write(b"\\");
    assert_eq!(vt.take_notification(), None);

    vt.write(b"\x1b]777;notify;Claude;Permission needed\x07");
    assert_eq!(
        vt.take_notification(),
        Some(TermNotification {
            title: "Claude".into(),
            body: "Permission needed".into(),
        })
    );
    assert_eq!(
        vt.take_notification(),
        None,
        "OSC's BEL terminator is not another bell"
    );
}

/// Agent session updates ride the notification channel: the app reads the
/// body prefix and records the id instead of showing it.
#[test]
fn a_session_update_keeps_its_agent_and_prefix() {
    let mut vt = Vt::new(80, 24);
    vt.write(b"\x1b]777;notify;codex;kumu-session:abc-1\x07");
    assert_eq!(
        vt.take_notification(),
        Some(TermNotification {
            title: "codex".into(),
            body: "kumu-session:abc-1".into(),
        })
    );
}

#[test]
fn terminal_titles_progress_and_bells_do_not_request_attention() {
    let mut vt = Vt::new(80, 24);
    vt.write(b"\x1b]2;Terminal title\x07\x1b]9;4;1;50\x07");
    assert_eq!(vt.take_notification(), None);
    vt.write(b"\x07\x07");
    assert_eq!(vt.take_notification(), None);
    vt.write(b"\x1b]9;Ready\x07\x07");
    assert_eq!(
        vt.take_notification().expect("OSC notification").body,
        "Ready"
    );
    assert_eq!(
        vt.take_notification(),
        None,
        "a following bell must not create another alert"
    );
}

#[test]
fn a_program_title_and_reported_pwd_reach_the_snapshot() {
    let mut vt = Vt::new(40, 4);
    vt.write(b"\x1b]2;Claude Code\x07");
    let snapshot = vt.snapshot("zsh", "/launch", false);
    assert_eq!(snapshot.title, "Claude Code");
    assert_eq!(snapshot.label(), "Claude Code");
    assert_eq!(snapshot.cwd, "/launch", "no OSC 7: the launch directory");

    // OSC 7 carries a URI and percent-encodes the path.
    vt.write(b"\x1b]7;file://localhost/Users/me/kumu%20demo\x07");
    let snapshot = vt.snapshot("zsh", "/launch", false);
    assert_eq!(snapshot.cwd, "/Users/me/kumu demo");

    // iTerm2's OSC 1337 carries a bare path.
    vt.write(b"\x1b]1337;CurrentDir=/tmp/demo\x07");
    assert_eq!(vt.snapshot("zsh", "/launch", false).cwd, "/tmp/demo");
}

#[test]
fn an_osc8_hyperlink_covers_the_cells_it_was_written_with() {
    let mut vt = Vt::new(40, 4);
    // "click here" is the visible text; the URI is the link behind it.
    vt.write(b"\x1b]8;;https://example.com/docs\x1b\\click here\x1b]8;;\x1b\\");
    let link = vt.hyperlink_at(3, 0).expect("hyperlink under the text");
    assert_eq!(link.uri, "https://example.com/docs");
    assert_eq!((link.row, link.from, link.to), (0, 0, 10));
    assert_eq!(vt.hyperlink_at(20, 0), None, "the rest of the row is plain");
    let snapshot = vt.snapshot("t", "/", false);
    assert!(cell(&snapshot, 0, 0).hyperlink);
    assert!(!cell(&snapshot, 20, 0).hyperlink);
}

#[test]
fn a_soft_wrapped_row_says_it_continues_below() {
    let mut vt = Vt::new(10, 4);
    vt.write(b"0123456789abcdef");
    let snapshot = vt.snapshot("t", "/", false);
    assert!(snapshot.wrap[0]);
    assert!(!snapshot.wrap[1]);
    assert_eq!(cell(&snapshot, 0, 1).text, "a");
}

#[test]
fn red_text_lands_in_the_first_cells() {
    let mut vt = Vt::new(16, 4);
    vt.write(b"\x1b[31mhi");
    let snapshot = vt.snapshot("t", "/", false);
    assert_eq!(cell(&snapshot, 0, 0).text, "h");
    assert_eq!(cell(&snapshot, 1, 0).text, "i");
    let fg = cell(&snapshot, 0, 0).fg;
    assert!(fg.0 > fg.1 && fg.0 > fg.2, "{fg:?}");
}

#[test]
fn a_device_attributes_query_is_answered() {
    let mut vt = Vt::new(16, 4);
    vt.write(b"\x1b[c");
    let reply = vt.take_replies();
    let reply = String::from_utf8_lossy(&reply);
    assert!(
        reply.starts_with("\x1b[?")
            && reply.ends_with('c')
            && reply[3..reply.len() - 1]
                .split(';')
                .all(|field| field.parse::<u16>().is_ok()),
        "DA1 reply {reply:?}"
    );
}

#[test]
fn synchronized_output_holds_the_frame_until_release() {
    let mut vt = Vt::new(16, 4);
    vt.write(b"A");
    assert_eq!(cell(&vt.snapshot("t", "/", false), 0, 0).text, "A");
    vt.write(b"\x1b[?2026h");
    vt.write(b"B");
    assert_eq!(
        cell(&vt.snapshot("t", "/", false), 1, 0).text,
        "",
        "the held frame stays on A"
    );
    vt.write(b"\x1b[?2026l");
    assert_eq!(cell(&vt.snapshot("t", "/", false), 1, 0).text, "B");
}

#[test]
fn completion_keys_encode_for_the_shell() {
    let mut vt = Vt::new(16, 4);
    let mut input = KeyInput {
        key: "a".into(),
        key_char: Some("a".into()),
        shift: false,
        control: false,
        alt: false,
    };
    assert_eq!(vt.encode_input(&input), b"a", "a plain letter is itself");
    input.key = "tab".into();
    input.key_char = Some("\t".into());
    assert_eq!(vt.encode_input(&input), b"\t");
    input.shift = true;
    assert_eq!(vt.encode_input(&input), b"\x1b[Z");
}

#[test]
fn a_drag_selects_plain_text() {
    let mut vt = Vt::new(16, 4);
    vt.write(b"hello");
    vt.select(SelectStep::Press, 0, 0, 1.0, 5.0, 10.0, 40.0);
    vt.select(SelectStep::Drag, 5, 0, 50.0, 5.0, 10.0, 40.0);
    vt.select(SelectStep::Release, 5, 0, 50.0, 5.0, 10.0, 40.0);
    assert_eq!(vt.selection_text().as_deref(), Some("hello"));
    assert!(vt.clear_selection(), "a selection was there to clear");
    assert_eq!(vt.selection_text(), None);
    assert!(!vt.clear_selection(), "clearing again changes nothing");
}

#[test]
fn search_finds_matches_and_reports_their_viewport_rows() {
    let mut vt = Vt::new(20, 6);
    for line in [
        "first alpha",
        "beta",
        "second alpha",
        "gamma",
        "third alpha",
    ] {
        vt.write(line.as_bytes());
        vt.write(b"\r\n");
    }
    vt.search_set("alpha");
    let snapshot = vt.snapshot("sh", "/", false);
    let search = snapshot.search.clone().expect("a search with a needle");
    assert_eq!(search.total, 3);
    assert_eq!(search.selected, Some(0), "the newest match is selected");
    assert!(!search.hits.is_empty());

    let mut selected = 0;
    for hit in &search.hits {
        let row = &snapshot.cells[hit.row as usize];
        let text: String = row[hit.from as usize..hit.to as usize]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(text, "alpha", "hit {hit:?} covers {text:?}");
        if hit.selected {
            selected += 1;
        }
    }
    assert_eq!(selected, 1, "one visible run is the selected match");
}

#[test]
fn search_steps_toward_older_matches_and_wraps() {
    let mut vt = Vt::new(20, 6);
    for line in ["alpha one", "beta", "alpha two", "gamma", "alpha three"] {
        vt.write(line.as_bytes());
        vt.write(b"\r\n");
    }
    vt.search_set("alpha");
    let selected = |vt: &mut Vt| {
        vt.snapshot("sh", "/", false)
            .search
            .expect("search view")
            .selected
    };
    assert_eq!(selected(&mut vt), Some(0));
    assert!(vt.search_step(true));
    assert_eq!(selected(&mut vt), Some(1));
    assert!(vt.search_step(true));
    assert_eq!(selected(&mut vt), Some(2));
    assert!(vt.search_step(true), "after the oldest, wraps to newest");
    assert_eq!(selected(&mut vt), Some(0));
    assert!(vt.search_step(false));
    assert_eq!(selected(&mut vt), Some(2), "back toward newer content");
}

#[test]
fn clearing_the_needle_removes_search_highlights() {
    let mut vt = Vt::new(20, 4);
    vt.write(b"alpha\r\n");
    vt.search_set("alpha");
    assert!(vt.snapshot("sh", "/", false).search.is_some());
    assert!(vt.search_clear());
    let snapshot = vt.snapshot("sh", "/", false);
    assert!(snapshot.search.is_none());
}

#[test]
fn search_reads_scrollback_beyond_the_visible_rows() {
    let mut vt = Vt::new(20, 4);
    for line in 0..40 {
        vt.write(format!("line {line} alpha\r\n").as_bytes());
    }
    vt.search_set("alpha");
    let snapshot = vt.snapshot("sh", "/", false);
    let search = snapshot.search.expect("search view");
    assert_eq!(search.total, 40);
    for hit in &search.hits {
        assert!(hit.row < snapshot.rows, "hit {hit:?} outside the viewport");
    }
}

#[test]
fn content_text_puts_the_newest_output_after_the_scrollback() {
    let mut vt = Vt::new(20, 4);
    for line in 0..10 {
        vt.write(format!("line {line}\r\n").as_bytes());
    }
    let all = vt.content_text(usize::MAX);
    let rows: Vec<usize> = all
        .lines()
        .map(|line| line.trim_start_matches("line ").parse().expect("a row"))
        .collect();
    assert!(
        rows.windows(2).all(|pair| pair[0] + 1 == pair[1]),
        "oldest first, newest last: {all:?}"
    );
    assert_eq!(rows.last(), Some(&9), "{all:?}");

    // A smaller history keeps only the newest scrollback rows above the
    // screen; the screen itself is always all there.
    let tail = vt.content_text(2);
    assert!(all.ends_with(&tail), "the tail is a suffix of {all:?}");
    assert!(tail.lines().count() < rows.len(), "{tail:?}");
    assert_eq!(tail.lines().last(), Some("line 9"), "{tail:?}");
}

#[test]
fn content_text_reads_the_tail_whatever_the_viewport_shows() {
    let mut vt = Vt::new(20, 4);
    for line in 0..20 {
        vt.write(format!("line {line}\r\n").as_bytes());
    }
    vt.scroll_to_row(0);
    let before = vt.scrollbar().expect("scrollback").offset;

    let text = vt.content_text(0);
    assert!(text.contains("line 19"), "{text:?}");
    assert_eq!(
        vt.scrollbar().expect("scrollback").offset,
        before,
        "the user's scroll position is put back"
    );
}

/// A find bar restarts the search on every keystroke, so one search over a
/// full scrollback must not block the session thread for long.
#[test]
fn searching_a_full_scrollback_stays_interactive() {
    let mut vt = Vt::new(100, 40);
    for line in 0..10_000 {
        vt.write(format!("line {line} with some words on it\r\n").as_bytes());
    }
    let started = std::time::Instant::now();
    vt.search_set("words");
    let elapsed = started.elapsed();
    let total = vt
        .snapshot("sh", "/", false)
        .search
        .expect("search view")
        .total;
    assert!(
        total > 0,
        "the scrollback holds matches, search found {total}"
    );
    // This guards against a pathological search, not a latency budget: a
    // loaded CI machine must not turn it into a flaky benchmark.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "a keystroke waited {elapsed:?} for the search"
    );
}

#[test]
fn a_paste_is_framed_when_the_program_asks_for_it() {
    let mut vt = Vt::new(16, 4);
    // No mode 2004: typing, so a newline is a carriage return.
    assert_eq!(vt.encode_paste(b"echo hi\n"), b"echo hi\r");
    vt.write(b"\x1b[?2004h");
    assert_eq!(
        vt.encode_paste(b"echo hi\nls\n"),
        b"\x1b[200~echo hi\nls\n\x1b[201~"
    );
}

#[test]
fn a_mouse_report_carries_modifiers() {
    let mut vt = Vt::new(16, 4);
    vt.write(b"\x1b[?1002h\x1b[?1006h");
    let bytes = vt.encode_mouse(MouseInput {
        action: MouseAction::Press,
        button: Some(MouseButton::Left),
        x: 5.0,
        y: 5.0,
        cell_w: 10.0,
        cell_h: 20.0,
        any_pressed: true,
        mods: MouseMods {
            shift: false,
            control: true,
            alt: false,
        },
    });
    assert_eq!(bytes, b"\x1b[<16;1;1M");
}

#[test]
fn apply_colors_preserves_the_extended_256_color_palette() {
    let mut vt = Vt::new(20, 4);
    // Query xterm palette index 16 (cube origin, black 0000/0000/0000) and
    // 196 (bright red ffff/0000/0000). A 16-entry buffer over-read would
    // fill these with unrelated stack bytes.
    vt.write(b"\x1b]4;16;?\x07\x1b]4;196;?\x07");
    let reply = String::from_utf8_lossy(&vt.take_replies()).into_owned();
    assert!(
        reply.contains("4;16;rgb:0000/0000/0000"),
        "unexpected index 16 reply: {reply:?}"
    );
    assert!(
        reply.contains("4;196;rgb:ffff/0000/0000"),
        "unexpected index 196 reply: {reply:?}"
    );
}

#[test]
fn reported_path_rejects_foreign_hosts_and_control_bytes() {
    assert_eq!(
        reported_path("file://localhost/tmp/work").as_deref(),
        Some("/tmp/work")
    );
    assert_eq!(
        reported_path("file:///tmp/work").as_deref(),
        Some("/tmp/work")
    );
    assert_eq!(reported_path("file://remote.example.com/etc"), None);
    assert_eq!(reported_path("file:///tmp/a%00b"), None);
}

#[test]
fn plain_text_and_content_text_keep_interior_spaces() {
    let mut vt = Vt::new(20, 4);
    vt.write(b"foo\x1b[3Cbar");
    let snap = vt.snapshot("sh", "/", false);
    assert!(
        snap.plain_text().contains("foo   bar"),
        "{}",
        snap.plain_text()
    );
    assert_eq!(vt.content_text(0), "foo   bar");
}

#[test]
fn wrapped_search_match_highlights_the_start_column_accurately() {
    let mut vt = Vt::new(10, 4);
    vt.write(b"1234567abcdef");
    vt.search_set("abcdef");
    let search = vt.snapshot("sh", "/", false).search.expect("search");
    assert_eq!(search.hits.len(), 2);
    assert_eq!(
        (search.hits[0].row, search.hits[0].from, search.hits[0].to),
        (0, 7, 10)
    );
    assert_eq!(
        (search.hits[1].row, search.hits[1].from, search.hits[1].to),
        (1, 0, 3)
    );
}
