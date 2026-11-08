//! Titles agents write to their own on-disk session stores.
//!
//! Home sessions are interactive TUIs, so there is no protocol `SessionTitle`
//! event. Each agent still persists a name (Claude under `~/.claude/sessions`,
//! Codex in `session_index.jsonl`, OpenCode in `opencode.db`, Pi under
//! `~/.pi/agent/sessions`); this module reads those and scores a match against
//! a kumu terminal's cwd and, when known, its process id.
//!
//! A *shell* session can claim any agent's title: the shell's process tree
//! tells us which agent TUI is running under its PTY and when that process
//! started, and only sessions written after that start are considered — a cwd
//! match alone would let a new shell inherit an older session in the same
//! directory. Claude additionally writes its pid into the live session file,
//! so its own binding is exact.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// A title found on disk that may belong to a kumu terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskTitle {
    pub agent: &'static str,
    /// The `agent:id` binding used to look this title up again.
    pub agent_session_id: String,
    pub title: String,
    /// Higher wins: an exact PID hit outranks a cwd-only match, and within the
    /// same tier a more recently updated agent session wins.
    pub score: i64,
    /// Session write time in Unix seconds; compared with the agent process
    /// start when a shell claims a title.
    pub updated: i64,
}

/// Agent session ids we persist are `agent:id` so a later lookup knows which
/// store to open even when the kumu session is still a plain shell that
/// launched the agent by hand.
pub fn encode_resume(agent: &str, id: &str) -> String {
    format!("{agent}:{id}")
}

pub fn decode_resume(resume: &str) -> Option<(&str, &str)> {
    resume.split_once(':').filter(|(agent, id)| {
        !agent.is_empty()
            && !id.is_empty()
            && matches!(*agent, "claude" | "codex" | "opencode" | "pi")
    })
}

/// Resolve a previously bound resume id back to its current title.
pub fn lookup(resume: &str) -> Option<String> {
    let (agent, id) = decode_resume(resume)?;
    match agent {
        "claude" => claude_by_id(id),
        "codex" => codex_by_id(id),
        "opencode" => opencode_by_id(id),
        "pi" => pi_by_id(id),
        _ => None,
    }
}

/// Discover titles that may belong to a terminal in `cwd`.
///
/// `prefer` limits the search when the session already names an agent; a shell
/// session (or `None`) asks every store. When `process_bound` is set, titles
/// are accepted only when the agent process running under the terminal (or a
/// child of it) started before the session was last written, and an exact pid
/// in Claude's session file outranks that. A bare cwd match is never enough, so
/// a newly opened shell does not inherit an older agent session's name.
///
/// Results are unsorted — callers pick by [`DiskTitle::score`] and exclusive
/// resume claiming.
pub fn discover(
    cwd: &str,
    pid: Option<u32>,
    prefer: Option<&str>,
    process_bound: bool,
) -> Vec<DiskTitle> {
    let cwd = normalize_cwd(cwd);
    if cwd.is_empty() {
        return Vec::new();
    }
    if process_bound {
        return process_bound_titles(&cwd, pid);
    }
    let mut found = Vec::new();
    let agents: &[&str] = match prefer {
        Some("claude") => &["claude"],
        Some("codex") => &["codex"],
        Some("opencode") => &["opencode"],
        Some("pi") | Some("omp") => &["pi"],
        _ => &["claude", "codex", "opencode", "pi"],
    };
    for agent in agents {
        found.extend(discover_store(agent, &cwd, pid));
    }
    found
}

/// Every store's sessions for `agent` in `cwd`, with no process filter.
///
/// `pid` only strengthens Claude's score when the live session file names that
/// process; other stores ignore it.
fn discover_store(agent: &str, cwd: &str, pid: Option<u32>) -> Vec<DiskTitle> {
    match agent {
        "claude" => claude_discover(cwd, pid, false),
        "codex" => codex_discover(cwd),
        "opencode" => opencode_discover(cwd),
        "pi" => pi_discover(cwd),
        _ => Vec::new(),
    }
}

/// Titles a shell terminal may claim: one whose agent runs under that PTY.
fn process_bound_titles(cwd: &str, pid: Option<u32>) -> Vec<DiskTitle> {
    let Some(pid) = pid else {
        return Vec::new();
    };
    let tree = process_tree(&process_snapshot(), pid);
    // No child process and no agent at the root: nothing can be running under
    // this terminal, so every agent store would only be walked to find
    // nothing. This is the common case for an idle shell, which is polled every
    // two seconds. (The root still counts as the agent when the shell `exec`s
    // one directly.)
    if tree.pids.len() == 1 && tree.agents.is_empty() {
        return Vec::new();
    }
    let mut found = claude_discover_under(cwd, Some(&tree), true);
    for &(agent, started) in &tree.agents {
        // Claude already answered above by pid; a looser cwd-tier candidate
        // for the same store would only risk claiming a stale file.
        if agent == "claude" {
            continue;
        }
        for mut title in discover_store(agent, cwd, None) {
            if !covers_start(title.updated, started) {
                continue;
            }
            title.score = PROCESS_SCORE + title.updated;
            found.push(title);
        }
    }
    found
}

/// Whether a session write at `updated` can belong to an agent that started at
/// `started`. The skew absorbs whole-second `ps` clocks and the gap before the
/// TUI's first write.
fn covers_start(updated: i64, started: i64) -> bool {
    updated + START_SKEW_SECS >= started
}

/// Whether a previously bound resume id still belongs to this terminal.
///
/// Claude is proved by its pid file: the binding keeps only while that pid is
/// the terminal or a descendant. The other agents record no pid, so they are
/// proved by an agent process of the same kind under this terminal plus a
/// session written after that process started. A stale binding is rejected so
/// a new shell does not keep someone else's name.
pub fn resume_belongs_to_terminal(resume: &str, term_pid: Option<u32>) -> bool {
    let Some(term_pid) = term_pid else {
        return false;
    };
    let Some((agent, id)) = decode_resume(resume) else {
        return false;
    };
    match agent {
        "claude" => {
            claude_pid_for(id).is_some_and(|pid| process_tree_pids(term_pid).contains(&pid))
        }
        "codex" | "opencode" | "pi" => {
            let Some(started) = agent_started_under(term_pid, agent) else {
                return false;
            };
            let updated = match agent {
                "codex" => codex_updated(id),
                "opencode" => opencode_updated(id),
                "pi" => pi_updated(id),
                _ => None,
            };
            updated.is_some_and(|updated| covers_start(updated, started))
        }
        _ => false,
    }
}

/// When an `agent` process under `term_pid` started, in Unix seconds.
///
/// The earliest such process wins: the TUI itself is an ancestor of any helper
/// it spawns, and a session must have been written after the TUI started.
fn agent_started_under(term_pid: u32, agent: &str) -> Option<i64> {
    process_tree(&process_snapshot(), term_pid)
        .agents
        .into_iter()
        .find(|(a, _)| *a == agent)
        .map(|(_, started)| started)
}

fn normalize_cwd(cwd: &str) -> String {
    let path = Path::new(cwd);
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| {
            let trimmed = cwd.trim_end_matches('/');
            if trimmed.is_empty() {
                cwd.to_string()
            } else {
                trimmed.to_string()
            }
        })
}

std::thread_local! {
    /// Test-only override so fixtures do not fight over the process `HOME`.
    static HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn home() -> Option<PathBuf> {
    HOME_OVERRIDE
        .with(|cell| cell.borrow().clone())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

fn title_of(raw: &str) -> Option<String> {
    let title = raw.trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn mtime_score(path: &Path) -> i64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --- Claude Code -----------------------------------------------------------
//
// Live interactive sessions write `~/.claude/sessions/<pid>.json` with `name`,
// `cwd`, `sessionId`, and `pid`. Project transcripts under `projects/` are the
// long history; the pid file is what tracks the running TUI's display name.

fn claude_sessions_dir() -> Option<PathBuf> {
    Some(home()?.join(".claude").join("sessions"))
}

/// Score tier for an agent process seen under the terminal: it outranks any
/// recency value, so a cwd-only match with a fresher file can never take a
/// terminal that a real process binding owns. Within a tier, recency decides.
///
/// Larger than any timestamp the stores produce, including the millisecond
/// epoch OpenCode keeps.
const PROCESS_SCORE: i64 = 1 << 50;

/// How far a session's write time may trail an agent process's start before we
/// stop believing it belongs to that process. `ps` reports whole seconds, and
/// the TUI may have been spawned a moment before its first write.
const START_SKEW_SECS: i64 = 5;

fn claude_discover(cwd: &str, pid: Option<u32>, process_bound: bool) -> Vec<DiskTitle> {
    let tree = match (pid, process_bound) {
        (Some(pid), true) => Some(process_tree(&process_snapshot(), pid)),
        // Exact pid-file match against this terminal without a full `ps`.
        (Some(pid), false) => Some(process_tree(&[], pid)),
        (None, _) => None,
    };
    claude_discover_under(cwd, tree.as_ref(), process_bound)
}

fn claude_discover_under(
    cwd: &str,
    tree: Option<&ProcessTree>,
    process_bound: bool,
) -> Vec<DiskTitle> {
    let Some(dir) = claude_sessions_dir() else {
        return Vec::new();
    };
    let cwd = normalize_cwd(cwd);
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(session_cwd) = value.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if normalize_cwd(session_cwd) != cwd {
            continue;
        }
        let Some(title) = value.get("name").and_then(Value::as_str).and_then(title_of) else {
            continue;
        };
        let Some(id) = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let file_pid = value.get("pid").and_then(Value::as_u64).map(|p| p as u32);
        let under_terminal = match (tree, file_pid) {
            (Some(tree), Some(have)) => tree.pids.contains(&have),
            _ => false,
        };
        if process_bound && !under_terminal {
            continue;
        }
        let updated = mtime_score(&path);
        let mut score = updated;
        if under_terminal {
            score += PROCESS_SCORE;
        }
        found.push(DiskTitle {
            agent: "claude",
            agent_session_id: encode_resume("claude", id),
            title,
            score,
            updated,
        });
    }
    found
}

/// The live `~/.claude/sessions/<pid>.json` for `id`, if Claude still has one.
fn claude_live_session(id: &str) -> Option<(PathBuf, Value)> {
    let dir = claude_sessions_dir()?;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("sessionId").and_then(Value::as_str) == Some(id) {
            return Some((path, value));
        }
    }
    None
}

fn claude_pid_for(id: &str) -> Option<u32> {
    claude_live_session(id)?
        .1
        .get("pid")
        .and_then(Value::as_u64)
        .map(|p| p as u32)
}

/// `root` and every process descended from it, via one `ps` snapshot.
fn process_tree_pids(root: u32) -> std::collections::HashSet<u32> {
    process_tree(&process_snapshot(), root).pids
}

/// One row of the `ps` snapshot used to find the agent under a terminal.
struct Proc {
    pid: u32,
    ppid: u32,
    /// Unix seconds, derived from `ps`'s elapsed time.
    started: i64,
    /// The process's command line, split on whitespace.
    args: Vec<String>,
}

/// The processes under a terminal, and which agents they are.
struct ProcessTree {
    pids: std::collections::HashSet<u32>,
    /// Agent id → earliest start time among its processes.
    agents: Vec<(&'static str, i64)>,
}

/// One `ps` snapshot of every process, with its start time and command line.
///
/// `etime` (elapsed seconds) is used instead of a formatted start date because
/// the latter is locale-dependent; `args` instead of `comm` because macOS
/// truncates `comm` to 16 characters.
fn process_snapshot() -> Vec<Proc> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,etime=,args="])
        .output()
    else {
        return Vec::new();
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| parse_proc_line(line, now))
        .collect()
}

fn parse_proc_line(line: &str, now: i64) -> Option<Proc> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let ppid = fields.next()?.parse().ok()?;
    let elapsed = parse_elapsed(fields.next()?)?;
    Some(Proc {
        pid,
        ppid,
        started: now - elapsed,
        args: fields.map(str::to_string).collect(),
    })
}

/// `root`, its descendants, and the agent processes among them.
fn process_tree(procs: &[Proc], root: u32) -> ProcessTree {
    let mut pids = std::collections::HashSet::new();
    pids.insert(root);
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for proc_ in procs {
        children.entry(proc_.ppid).or_default().push(proc_.pid);
    }
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        let Some(kids) = children.get(&pid) else {
            continue;
        };
        for &kid in kids {
            if pids.insert(kid) {
                stack.push(kid);
            }
        }
    }
    let mut by_agent: std::collections::HashMap<&'static str, i64> =
        std::collections::HashMap::new();
    for proc_ in procs.iter().filter(|p| pids.contains(&p.pid)) {
        let Some(agent) = agent_of_process(&proc_.args) else {
            continue;
        };
        by_agent
            .entry(agent)
            .and_modify(|started| *started = (*started).min(proc_.started))
            .or_insert(proc_.started);
    }
    let mut agents: Vec<_> = by_agent.into_iter().collect();
    agents.sort_by_key(|(agent, _)| *agent);
    ProcessTree { pids, agents }
}

/// Which known agent a command line runs, if any.
///
/// The first word is the executable path; when it is a JavaScript runtime the
/// script it runs is checked too, because an npm-installed agent executes as
/// `node …/claude`.
fn agent_of_process(args: &[String]) -> Option<&'static str> {
    let name_of = |arg: &str| {
        Path::new(arg)
            .file_name()
            .map(|n| n.to_string_lossy().trim_start_matches('-').to_string())
    };
    let first = args.first().and_then(|a| name_of(a))?;
    if let Some(agent) = agent_by_name(&first) {
        return Some(agent);
    }
    if matches!(first.as_str(), "node" | "bun" | "deno") {
        return args.iter().skip(1).find_map(|arg| {
            let name = name_of(arg)?;
            agent_by_name(&name)
        });
    }
    None
}

fn agent_by_name(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        // `omp` is Oh My Pi and keeps its sessions in Pi's store.
        "pi" | "omp" => Some("pi"),
        _ => None,
    }
}

/// Parse `ps`'s elapsed time: `MM:SS`, `HH:MM:SS`, or `D-HH:MM:SS`.
fn parse_elapsed(text: &str) -> Option<i64> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<i64>().ok()?, rest),
        None => (0, text),
    };
    let mut seconds = 0i64;
    for part in rest.split(':') {
        seconds = seconds.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    Some(days * 86_400 + seconds)
}

fn claude_by_id(id: &str) -> Option<String> {
    if let Some((_, value)) = claude_live_session(id) {
        return value.get("name").and_then(Value::as_str).and_then(title_of);
    }
    // Fall back to the project transcript's first user line when the live
    // pid file is gone — still better than leaving "终端 N".
    claude_transcript_title(id)
}

fn claude_transcript_path(id: &str) -> Option<PathBuf> {
    let projects = home()?.join(".claude").join("projects");
    for project in fs::read_dir(projects).ok()?.flatten() {
        let path = project.path().join(format!("{id}.jsonl"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn claude_transcript_title(id: &str) -> Option<String> {
    let text = fs::read_to_string(claude_transcript_path(id)?).ok()?;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let content = value.pointer("/message/content").and_then(|c| {
            c.as_str().map(|s| s.to_string()).or_else(|| {
                c.as_array().and_then(|parts| {
                    parts.iter().find_map(|part| {
                        part.get("text")
                            .and_then(Value::as_str)
                            .map(|s| s.to_string())
                    })
                })
            })
        })?;
        return first_line_title(&content);
    }
    None
}

fn first_line_title(text: &str) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut title: String = line.chars().take(48).collect();
    if line.chars().count() > 48 {
        title.push('…');
    }
    title_of(&title)
}

// --- Codex -----------------------------------------------------------------
//
// `~/.codex/session_index.jsonl` holds `thread_name` by id. The matching cwd
// lives on the first line (`session_meta`) of each rollout under
// `~/.codex/sessions/YYYY/MM/DD/`.

fn codex_home() -> Option<PathBuf> {
    Some(home()?.join(".codex"))
}

fn codex_discover(cwd: &str) -> Vec<DiskTitle> {
    let Some(root) = codex_home() else {
        return Vec::new();
    };
    let names = codex_thread_names(&root.join("session_index.jsonl"));
    if names.is_empty() {
        return Vec::new();
    }
    let cwd = normalize_cwd(cwd);
    let mut found = Vec::new();
    let sessions = root.join("sessions");
    for path in recent_files(&sessions, 80) {
        let Some(meta) = codex_file_meta(&path) else {
            continue;
        };
        let Some(session_cwd) = meta.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if normalize_cwd(session_cwd) != cwd {
            continue;
        }
        let Some(id) = codex_meta_id(&meta) else {
            continue;
        };
        let Some(title) = names.get(&id).cloned() else {
            continue;
        };
        let updated = mtime_score(&path);
        found.push(DiskTitle {
            agent: "codex",
            agent_session_id: encode_resume("codex", &id),
            title,
            score: updated,
            updated,
        });
    }
    found
}

fn codex_by_id(id: &str) -> Option<String> {
    let root = codex_home()?;
    codex_thread_names(&root.join("session_index.jsonl")).remove(id)
}

fn codex_updated(id: &str) -> Option<i64> {
    // Same clock discover uses: the rollout file's mtime. Index `updated_at`
    // can lag an active thread, so it is not a fallback here.
    let root = codex_home()?;
    for path in recent_files(&root.join("sessions"), 80) {
        let Some(meta) = codex_file_meta(&path) else {
            continue;
        };
        if codex_meta_id(&meta).as_deref() == Some(id) {
            return Some(mtime_score(&path));
        }
    }
    None
}

fn codex_file_meta(path: &Path) -> Option<Value> {
    use std::io::BufRead;
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let meta = serde_json::from_str::<Value>(first.trim()).ok()?;
    Some(meta.get("payload").cloned().unwrap_or(meta))
}

fn codex_meta_id(meta: &Value) -> Option<String> {
    meta.get("session_id")
        .or_else(|| meta.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn codex_thread_names(path: &Path) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return names;
    };
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(title) = value
            .get("thread_name")
            .and_then(Value::as_str)
            .and_then(title_of)
        else {
            continue;
        };
        names.insert(id.to_string(), title);
    }
    names
}

fn recent_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    walk(root, &mut files);
    files.sort_by_key(|p| std::cmp::Reverse(mtime_score(p)));
    files.truncate(limit);
    files
}

// --- OpenCode --------------------------------------------------------------
//
// `~/.local/share/opencode/opencode.db` table `session_v2` has `title` and
// `directory`.

fn opencode_db() -> Option<PathBuf> {
    Some(home()?.join(".local/share/opencode/opencode.db"))
}

fn opencode_discover(cwd: &str) -> Vec<DiskTitle> {
    let Some(path) = opencode_db() else {
        return Vec::new();
    };
    if !path.is_file() {
        return Vec::new();
    }
    let Some(conn) = opencode_connection() else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, title, time_updated FROM session_v2
         WHERE directory = ?1 AND title IS NOT NULL AND trim(title) != ''
         ORDER BY time_updated DESC LIMIT 8",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([cwd], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(id, title, updated)| {
            let title = title_of(&title)?;
            // `time_updated` is a millisecond epoch; the rest of the module
            // ranks and compares in seconds.
            let updated = updated / 1000;
            Some(DiskTitle {
                agent: "opencode",
                agent_session_id: encode_resume("opencode", &id),
                title,
                score: updated,
                updated,
            })
        })
        .collect()
}

fn opencode_by_id(id: &str) -> Option<String> {
    let conn = opencode_connection()?;
    conn.query_row("SELECT title FROM session_v2 WHERE id = ?1", [id], |row| {
        row.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
    .and_then(|t| title_of(&t))
}

fn opencode_updated(id: &str) -> Option<i64> {
    let conn = opencode_connection()?;
    let updated: i64 = conn
        .query_row(
            "SELECT time_updated FROM session_v2 WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .ok()?;
    Some(updated / 1000)
}

fn opencode_connection() -> Option<rusqlite::Connection> {
    let path = opencode_db()?;
    rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

// --- Pi --------------------------------------------------------------------
//
// `~/.pi/agent/sessions/--<cwd-encoded>--/<timestamp>_<id>.jsonl`. The display
// name is the latest `session_info` entry's `name`.

fn pi_sessions_root() -> Option<PathBuf> {
    Some(home()?.join(".pi").join("agent").join("sessions"))
}

fn pi_cwd_dir(cwd: &str) -> Option<PathBuf> {
    let encoded = format!(
        "--{}--",
        cwd.trim_start_matches('/').replace(['/', '\\', ':'], "-")
    );
    Some(pi_sessions_root()?.join(encoded))
}

fn pi_discover(cwd: &str) -> Vec<DiskTitle> {
    let Some(dir) = pi_cwd_dir(cwd) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some((id, title)) = pi_file_title(&path) else {
            continue;
        };
        let updated = mtime_score(&path);
        found.push(DiskTitle {
            agent: "pi",
            agent_session_id: encode_resume("pi", &id),
            title,
            score: updated,
            updated,
        });
    }
    found
}

fn pi_by_id(id: &str) -> Option<String> {
    let path = pi_file_for(id)?;
    pi_file_title(&path).map(|(_, title)| title)
}

fn pi_updated(id: &str) -> Option<i64> {
    Some(mtime_score(&pi_file_for(id)?))
}

/// The session file whose id is `id`, across every cwd directory.
fn pi_file_for(id: &str) -> Option<PathBuf> {
    let root = pi_sessions_root()?;
    let dirs = fs::read_dir(root).ok()?;
    for dir in dirs.flatten() {
        let path = dir.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let name = path.file_name()?.to_string_lossy();
            if !name.contains(id) || path.extension()?.to_str()? != "jsonl" {
                continue;
            }
            if let Some((found_id, _)) = pi_file_title(&path)
                && found_id == id
            {
                return Some(path);
            }
        }
    }
    None
}

fn pi_file_title(path: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(path).ok()?;
    let mut id = None;
    let mut name = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
            }
            Some("session_info") => {
                // An empty name clears the title; keep scanning so a later
                // non-empty entry can restore one.
                name = value.get("name").and_then(Value::as_str).and_then(title_of);
            }
            _ => {}
        }
    }
    Some((id?, name?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn agent_session_ids_round_trip_with_known_agents() {
        let encoded = encode_resume("claude", "abc");
        assert_eq!(decode_resume(&encoded), Some(("claude", "abc")));
        assert!(decode_resume("nope:x").is_none());
        assert!(decode_resume("claude:").is_none());
    }

    #[test]
    fn claude_pid_file_is_preferred_for_matching_cwd() {
        let dir = tempfile();
        let proj = dir.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_string_lossy().into_owned();
        let sessions = dir.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_json(
            &sessions.join("4242.json"),
            &serde_json::json!({
                "pid": 4242,
                "sessionId": "sess-a",
                "cwd": cwd,
                "name": "fix the parser",
            }),
        );
        write_json(
            &sessions.join("9999.json"),
            &serde_json::json!({
                "pid": 9999,
                "sessionId": "sess-b",
                "cwd": cwd,
                "name": "older name",
            }),
        );
        let _guard = HomeOverride::set(&dir);
        let found = claude_discover(&cwd, Some(4242), false);
        assert_eq!(found.len(), 2);
        // Exact pid match counts as process-bound even without walking children.
        let bound = claude_discover(&cwd, Some(4242), true);
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].title, "fix the parser");
        assert!(
            claude_discover(&cwd, Some(1), true).is_empty(),
            "a different terminal must not claim this pid's title"
        );
    }

    /// A process binding always outranks recency: a title in the same cwd whose
    /// file is much newer must not steal a terminal that owns its own agent.
    #[test]
    fn a_pid_hit_beats_a_much_newer_cwd_match() {
        let dir = tempfile();
        let proj = dir.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_string_lossy().into_owned();
        let sessions = dir.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_json(
            &sessions.join("4242.json"),
            &serde_json::json!({
                "pid": 4242,
                "sessionId": "sess-a",
                "cwd": cwd,
                "name": "mine",
            }),
        );
        write_json(
            &sessions.join("9999.json"),
            &serde_json::json!({
                "pid": 9999,
                "sessionId": "sess-b",
                "cwd": cwd,
                "name": "newer",
            }),
        );
        let _guard = HomeOverride::set(&dir);
        let found = claude_discover(&cwd, Some(4242), false);
        let score = |title: &str| {
            found
                .iter()
                .find(|t| t.title == title)
                .unwrap_or_else(|| panic!("{title} missing"))
                .score
        };
        assert!(score("mine") >= PROCESS_SCORE, "the pid hit owns a tier");
        assert!(
            score("newer") < PROCESS_SCORE,
            "a cwd-only match never reaches it, however fresh its file"
        );
    }

    #[test]
    fn a_new_shell_does_not_inherit_a_cwd_only_title() {
        let dir = tempfile();
        let proj = dir.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_string_lossy().into_owned();
        let sessions = dir.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_json(
            &sessions.join("4242.json"),
            &serde_json::json!({
                "pid": 4242,
                "sessionId": "sess-a",
                "cwd": cwd,
                "name": "someone else's session",
            }),
        );
        let _guard = HomeOverride::set(&dir);
        let found = discover(&cwd, Some(1), None, true);
        assert!(
            found.is_empty(),
            "a fresh shell must not take a title just because the cwd matches"
        );
    }

    #[test]
    fn pi_reads_the_latest_session_info_name() {
        let dir = tempfile();
        let proj = dir.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_string_lossy().into_owned();
        let encoded = format!(
            "--{}--",
            cwd.trim_start_matches('/').replace(['/', '\\', ':'], "-")
        );
        let sessions = dir
            .join(".pi")
            .join("agent")
            .join("sessions")
            .join(&encoded);
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("2024-01-01T00-00-00_sess-1.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{{\"type\":\"session\",\"id\":\"sess-1\",\"cwd\":{}}}",
            serde_json::to_string(&cwd).unwrap()
        )
        .unwrap();
        writeln!(file, r#"{{"type":"session_info","id":"a","name":"first"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_info","id":"b","name":"refactor auth"}}"#
        )
        .unwrap();
        let _guard = HomeOverride::set(&dir);
        let found = pi_discover(&cwd);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "refactor auth");
        assert_eq!(found[0].agent_session_id, "pi:sess-1");
    }

    #[test]
    fn ps_lines_keep_their_start_time_and_command() {
        let proc_ = parse_proc_line(
            " 69523 69268 05:56 /Users/yankewei/.opencode/bin/opencode",
            10_000,
        )
        .unwrap();
        assert_eq!((proc_.pid, proc_.ppid), (69_523, 69_268));
        assert_eq!(proc_.started, 10_000 - 356);
        assert_eq!(
            agent_of_process(&proc_.args),
            Some("opencode"),
            "the executable's basename identifies the agent"
        );
    }

    #[test]
    fn only_sessions_written_after_the_agent_started_are_claimed() {
        assert!(
            !covers_start(900, 1_000),
            "an older session in the same cwd is refused"
        );
        assert!(covers_start(2_000, 1_000));
        // Whole-second clocks and the gap before the first write are forgiven.
        assert!(covers_start(998, 1_000));
    }

    #[test]
    fn the_agent_process_under_a_terminal_is_recognized() {
        let procs = vec![
            Proc {
                pid: 1,
                ppid: 0,
                started: 0,
                args: vec!["-zsh".into()],
            },
            Proc {
                pid: 2,
                ppid: 1,
                started: 90,
                args: vec!["node".into(), "/usr/local/bin/opencode".into()],
            },
            Proc {
                pid: 3,
                ppid: 0,
                started: 0,
                args: vec!["/opt/homebrew/bin/codex".into()],
            },
        ];
        let tree = process_tree(&procs, 1);
        assert!(tree.pids.contains(&2));
        assert_eq!(
            tree.agents,
            vec![("opencode", 90)],
            "a sibling terminal's codex is not under this root"
        );
    }

    #[test]
    fn elapsed_times_parse_for_the_boundary_checks() {
        assert_eq!(parse_elapsed("00:05"), Some(5));
        assert_eq!(parse_elapsed("01:02:03"), Some(3_723));
        assert_eq!(parse_elapsed("1-01:02:03"), Some(86_400 + 3_723));
    }

    fn tempfile() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kumu-disk-title-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_string(value).unwrap()).unwrap();
    }

    struct HomeOverride;

    impl HomeOverride {
        fn set(path: &Path) -> Self {
            HOME_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path.to_path_buf()));
            Self
        }
    }

    impl Drop for HomeOverride {
        fn drop(&mut self) {
            HOME_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
        }
    }
}
