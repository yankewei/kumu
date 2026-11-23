use super::MAX_SCROLL_LINES;
use super::cli::{
    USAGE, call, caller_pane, flags, last_lines, pane_target, print_json, runtime_error,
    usage_error,
};
use super::protocol::{
    CloseResult, DEFAULT_WAIT_TIMEOUT_SECS, MAX_WAIT_TIMEOUT_SECS, PaneContent, PaneSummary,
    Request, ScrollDirection, ScrollResult, SendResult, SplitResult, WaitResult,
};

pub(super) fn panes_cli(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    if !rest.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    let value = match call(&Request::Panes) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("kumu: {error}");
            return 1;
        }
    };
    if json {
        let Some(panes) = value.get("panes").filter(|v| v.is_array()) else {
            eprintln!("kumu: unexpected reply: missing panes array");
            return 1;
        };
        println!("{panes}");
        return 0;
    }
    let panes: Vec<PaneSummary> = match serde_json::from_value(value["panes"].clone()) {
        Ok(panes) => panes,
        Err(error) => {
            eprintln!("kumu: unexpected reply: {error}");
            return 1;
        }
    };
    if panes.is_empty() {
        println!("no panes");
        return 0;
    }
    for pane in panes {
        println!("{}", summary_line(&pane));
    }
    0
}

pub(super) fn pane_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("read") => pane_read(&args[1..]),
        Some("scroll") => pane_scroll(&args[1..]),
        Some("split") => pane_split(&args[1..]),
        Some("send") => pane_send(&args[1..]),
        Some("wait") => pane_wait(&args[1..]),
        Some("close") => pane_close(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

fn pane_wait(args: &[String]) -> i32 {
    let mut target = None;
    let mut timeout_secs = DEFAULT_WAIT_TIMEOUT_SECS;
    let mut next = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--next" => next = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--timeout" | "-t" => match iter.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(secs) if (1..=MAX_WAIT_TIMEOUT_SECS).contains(&secs) => timeout_secs = secs,
                _ => {
                    return usage_error(&format!(
                        "--timeout needs seconds between 1 and {MAX_WAIT_TIMEOUT_SECS}"
                    ));
                }
            },
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            _ => return usage_error("one target at a time"),
        }
    }
    let Some(target) = target else {
        return usage_error("name the target pane");
    };
    let value = match call(&Request::Wait {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        timeout_secs,
        next,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let result: WaitResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&result);
    }
    println!(
        "{} · state {} · {:.1}s",
        summary_line(&result.pane),
        result.state,
        result.elapsed_ms as f64 / 1000.0
    );
    0
}

fn pane_close(args: &[String]) -> i32 {
    let mut target = None;
    let mut allow_self = false;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--self" => allow_self = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            _ => return usage_error("one target at a time"),
        }
    }
    let Some(target) = target else {
        return usage_error("name the target pane");
    };
    let value = match call(&Request::Close {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        allow_self,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let result: CloseResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&result);
    }
    println!("closed pane {}", result.pane);
    0
}

fn pane_read(args: &[String]) -> i32 {
    let mut target = None;
    let mut history = 0usize;
    let mut tail = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--history" | "-H" => match args.next().and_then(|n| n.parse().ok()) {
                Some(n) => history = n,
                None => return usage_error("--history needs a number"),
            },
            "--tail" | "-n" => match args.next().and_then(|n| n.parse().ok()) {
                Some(n) => tail = Some(n),
                None => return usage_error("--tail needs a number"),
            },
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other => {
                if target.is_some() {
                    return usage_error("one target at a time");
                }
                target = Some(other.to_string());
            }
        }
    }
    let target = pane_target(target);
    let value = match call(&Request::Read {
        pane: target,
        from: caller_pane(),
        history,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let mut content: PaneContent = match serde_json::from_value(value) {
        Ok(content) => content,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if let Some(tail) = tail {
        content.text = last_lines(&content.text, tail);
    }
    if json {
        return print_json(&content);
    }
    println!("{}", summary_line(&content.pane));
    println!("---");
    println!("{}", content.text.trim_end());
    0
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ScrollCliArgs {
    pub(super) direction: ScrollDirection,
    pub(super) target: Option<String>,
    pub(super) lines: Option<u16>,
    pub(super) col: Option<u16>,
    pub(super) row: Option<u16>,
    pub(super) json: bool,
}

pub(super) enum ScrollParse {
    Help,
    Parsed(ScrollCliArgs),
    Error(String),
}

pub(super) fn parse_pane_scroll(args: &[String]) -> ScrollParse {
    let mut direction = None;
    let mut target = None;
    let mut lines = None;
    let mut col = None;
    let mut row = None;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => return ScrollParse::Help,
            "--lines" | "-n" => match iter.next().and_then(|n| n.parse::<u16>().ok()) {
                Some(n) if (1..=MAX_SCROLL_LINES).contains(&n) => lines = Some(n),
                _ => {
                    return ScrollParse::Error(format!(
                        "--lines needs a number between 1 and {MAX_SCROLL_LINES}"
                    ));
                }
            },
            "--col" => match iter.next().and_then(|n| n.parse::<u16>().ok()) {
                Some(n) => col = Some(n),
                None => return ScrollParse::Error("--col needs a number".into()),
            },
            "--row" => match iter.next().and_then(|n| n.parse::<u16>().ok()) {
                Some(n) => row = Some(n),
                None => return ScrollParse::Error("--row needs a number".into()),
            },
            other if other.starts_with('-') => {
                return ScrollParse::Error(format!("unknown option {other}"));
            }
            other if direction.is_none() => match other {
                "up" => direction = Some(ScrollDirection::Up),
                "down" => direction = Some(ScrollDirection::Down),
                _ => {
                    return ScrollParse::Error("declare the direction first: up or down".into());
                }
            },
            other if target.is_none() => target = Some(other.to_string()),
            _ => return ScrollParse::Error("one target at a time".into()),
        }
    }
    let Some(direction) = direction else {
        return ScrollParse::Error("declare the direction: up or down".into());
    };
    ScrollParse::Parsed(ScrollCliArgs {
        direction,
        target,
        lines,
        col,
        row,
        json,
    })
}

fn pane_scroll(args: &[String]) -> i32 {
    let parsed = match parse_pane_scroll(args) {
        ScrollParse::Help => {
            println!("{USAGE}");
            return 0;
        }
        ScrollParse::Error(msg) => return usage_error(&msg),
        ScrollParse::Parsed(parsed) => parsed,
    };
    let value = match call(&Request::Scroll {
        pane: pane_target(parsed.target),
        from: caller_pane(),
        direction: parsed.direction,
        lines: parsed.lines,
        col: parsed.col,
        row: parsed.row,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let result: ScrollResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if parsed.json {
        return print_json(&result);
    }
    match result.moved {
        Some(moved) => println!(
            "scrolled pane {} {} {} lines (moved {moved})",
            result.pane.id,
            result.direction.as_str(),
            result.lines
        ),
        None => println!(
            "scrolled pane {} {} {} lines",
            result.pane.id,
            result.direction.as_str(),
            result.lines
        ),
    }
    0
}

fn pane_split(args: &[String]) -> i32 {
    let mut direction = None;
    let mut agent = None;
    let mut prompt: Option<String> = None;
    let mut trailing = Vec::new();
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--agent" | "-a" => match args.next() {
                Some(value) => agent = Some(value.clone()),
                None => return usage_error("--agent needs an id"),
            },
            "--prompt" | "-p" => match args.next() {
                Some(value) => prompt = Some(value.clone()),
                None => return usage_error("--prompt needs text"),
            },
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if direction.is_none() => direction = Some(other.to_string()),
            other => trailing.push(other.to_string()),
        }
    }
    let Some(direction) = direction else {
        return usage_error("declare the direction: right or down");
    };
    let Some(agent) = agent else {
        return usage_error("name the agent: --agent codex");
    };
    // Words left over after the direction read as the prompt, so quoting is
    // optional: `split right --agent codex review the diff`.
    if prompt.is_none() && !trailing.is_empty() {
        prompt = Some(trailing.join(" "));
    } else if prompt.is_some() && !trailing.is_empty() {
        return usage_error("the prompt is either --prompt TEXT or trailing words");
    }
    if prompt
        .as_deref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        return usage_error("--prompt needs text");
    }
    let value = match call(&Request::Split {
        direction,
        agent,
        from: caller_pane(),
        prompt,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let split: SplitResult = match serde_json::from_value(value) {
        Ok(split) => split,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&split);
    }
    println!("{}", summary_line(&split.pane));
    0
}

fn pane_send(args: &[String]) -> i32 {
    let mut target = None;
    let mut words = Vec::new();
    let mut enter = true;
    let mut json = false;
    // Everything after `--` is text, so it may start with a dash or be a word
    // that looks like a flag.
    let mut text_only = false;
    for arg in args {
        if text_only {
            words.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" if target.is_some() => text_only = true,
            "--json" => json = true,
            "--no-enter" => enter = false,
            "-h" | "--help" if words.is_empty() => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') && words.is_empty() => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            other => words.push(other.to_string()),
        }
    }
    let Some(target) = target else {
        return usage_error("name the pane to type into");
    };
    if words.is_empty() {
        return usage_error("give the text to type");
    }
    let value = match call(&Request::Send {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        text: words.join(" "),
        enter,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let send: SendResult = match serde_json::from_value(value) {
        Ok(send) => send,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&send);
    }
    println!("{}", summary_line(&send.pane));
    println!("{}", if send.submitted { "sent" } else { "typed" });
    0
}

fn summary_line(pane: &PaneSummary) -> String {
    let mut line = format!("pane {}", pane.id);
    line.push_str(&format!(" · {}", pane.project_name));
    if !pane.title.is_empty() {
        line.push_str(&format!(" · {}", pane.title));
    }
    if let Some(agent) = &pane.agent {
        line.push_str(&format!(" · agent {agent}"));
    }
    line.push_str(&format!(" · {}", pane.cwd));
    line.push_str(&format!(" · {}x{}", pane.cols, pane.rows));
    if pane.focused {
        line.push_str(" · focused");
    }
    if pane.selected {
        line.push_str(" · shown");
    }
    if pane.exited {
        line.push_str(" · exited");
    }
    line
}
