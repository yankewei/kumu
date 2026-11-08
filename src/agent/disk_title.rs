//! Titles agents write to their own on-disk session stores.
//!
//! Home sessions are interactive TUIs, so there is no protocol `SessionTitle`
//! event. Each agent still persists a name (Claude under `~/.claude/sessions`,
//! Codex in `session_index.jsonl`, OpenCode in `opencode.db`, Pi under
//! `~/.pi/agent/sessions`); this module reads those and scores a match against
//! a kumu terminal's cwd and, when known, its process id.
//!
//! Only Claude can be claimed by a *shell* session: its live session file
//! carries the agent's pid, so kumu can prove the TUI runs under that shell's
//! PTY. Codex, OpenCode and Pi record no pid, and a cwd match cannot tell the
//! agent running in this terminal from an older session in the same directory,
//! so their stores are only consulted when the session itself is the agent (a
//! non-shell session — which the app does not create today).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// A title found on disk that may belong to a kumu terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskTitle {
    pub agent: &'static str,
    pub resume_id: String,
    pub title: String,
    /// Higher wins: an exact PID hit outranks a cwd-only match, and within the
    /// same tier a more recently updated agent session wins.
    pub score: i64,
}

/// Resume ids we persist are `agent:id` so a later lookup knows which store to
/// open even when the kumu session is still a plain shell that launched the
/// agent by hand.
pub fn encode_resume(agent: &str, id: &str) -> String {
    format!("{agent}:{id}")
}

pub fn decode_resume(resume: &str) -> Option<(&str, &str)> {
    resume.split_once(':').filter(|(agent, id)| {
        !agent.is_empty() && !id.is_empty() && matches!(*agent, "claude" | "codex" | "opencode" | "pi")
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
/// session (or `None`) asks every store. When `process_bound` is set, only
/// titles whose agent process is this terminal (or a child of it) are returned
/// — a bare cwd match is not enough, so a newly opened shell does not inherit
/// an older agent session's name. That is also why a shell only ever finds
/// Claude (see the module notes).
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
    // A shell can only safely claim a title when we see the agent process
    // under its PTY. Claude is the one that writes a pid file; the others
    // are cwd-only and must wait until the session itself is that agent.
    if process_bound {
        return claude_discover(&cwd, pid, true);
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
        match *agent {
            "claude" => found.extend(claude_discover(&cwd, pid, false)),
            "codex" => found.extend(codex_discover(&cwd)),
            "opencode" => found.extend(opencode_discover(&cwd)),
            "pi" => found.extend(pi_discover(&cwd)),
            _ => {}
        }
    }
    found
}

/// Whether a previously bound resume id still belongs to this terminal.
///
/// Shell sessions only keep a Claude binding whose pid is the terminal or a
/// descendant; cwd-only claims (Codex / OpenCode / Pi, or a stale Claude file)
/// are rejected so a new shell does not keep someone else's name.
pub fn resume_belongs_to_terminal(resume: &str, term_pid: Option<u32>) -> bool {
    let Some(term_pid) = term_pid else {
        return false;
    };
    let Some((agent, id)) = decode_resume(resume) else {
        return false;
    };
    match agent {
        "claude" => claude_pid_for(id).is_some_and(|pid| {
            process_tree_pids(term_pid).contains(&pid)
        }),
        _ => false,
    }
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

fn claude_discover(cwd: &str, pid: Option<u32>, process_bound: bool) -> Vec<DiskTitle> {
    let Some(dir) = claude_sessions_dir() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return found;
    };
    // Only worth a `ps` snapshot when there are files to match against.
    let tree = pid.map(process_tree_pids);
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
        if normalize_cwd(session_cwd) != normalize_cwd(cwd) {
            continue;
        }
        let Some(title) = value
            .get("name")
            .and_then(Value::as_str)
            .and_then(title_of)
        else {
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
        let under_terminal = match (tree.as_ref(), file_pid) {
            (Some(tree), Some(have)) => tree.contains(&have),
            _ => false,
        };
        if process_bound && !under_terminal {
            continue;
        }
        let mut score = mtime_score(&path);
        if under_terminal {
            score += PROCESS_SCORE;
        }
        found.push(DiskTitle {
            agent: "claude",
            resume_id: encode_resume("claude", id),
            title,
            score,
        });
    }
    found
}

fn claude_pid_for(id: &str) -> Option<u32> {
    let dir = claude_sessions_dir()?;
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
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
        if value.get("sessionId").and_then(Value::as_str) != Some(id) {
            continue;
        }
        return value
            .get("pid")
            .and_then(Value::as_u64)
            .map(|p| p as u32);
    }
    None
}

/// `root` and every process descended from it, via one `ps` snapshot.
fn process_tree_pids(root: u32) -> std::collections::HashSet<u32> {
    let mut set = std::collections::HashSet::new();
    set.insert(root);
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
    else {
        return set;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut children: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|s| s.parse().ok()) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
    }
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        let Some(kids) = children.get(&pid) else {
            continue;
        };
        for &kid in kids {
            if set.insert(kid) {
                stack.push(kid);
            }
        }
    }
    set
}

fn claude_by_id(id: &str) -> Option<String> {
    let dir = claude_sessions_dir()?;
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
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
        if value.get("sessionId").and_then(Value::as_str) != Some(id) {
            continue;
        }
        return value
            .get("name")
            .and_then(Value::as_str)
            .and_then(title_of);
    }
    // Fall back to the project transcript's first user line when the live
    // pid file is gone — still better than leaving "终端 N".
    claude_transcript_title(id)
}

fn claude_transcript_title(id: &str) -> Option<String> {
    let projects = home()?.join(".claude").join("projects");
    let Ok(projects) = fs::read_dir(projects) else {
        return None;
    };
    for project in projects.flatten() {
        let path = project.path().join(format!("{id}.jsonl"));
        if !path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let content = value
                .pointer("/message/content")
                .and_then(|c| c.as_str().map(|s| s.to_string()).or_else(|| {
                    c.as_array().and_then(|parts| {
                        parts.iter().find_map(|part| {
                            part.get("text")
                                .and_then(Value::as_str)
                                .map(|s| s.to_string())
                        })
                    })
                }))?;
            return first_line_title(&content);
        }
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
    let mut found = Vec::new();
    let sessions = root.join("sessions");
    for path in recent_files(&sessions, 80) {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let mut first = String::new();
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(file);
        if reader.read_line(&mut first).is_err() {
            continue;
        }
        let Ok(meta) = serde_json::from_str::<Value>(first.trim()) else {
            continue;
        };
        let payload = meta.get("payload").unwrap_or(&meta);
        let Some(session_cwd) = payload.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if normalize_cwd(session_cwd) != normalize_cwd(cwd) {
            continue;
        }
        let Some(id) = payload
            .get("session_id")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(title) = names.get(id).cloned() else {
            continue;
        };
        found.push(DiskTitle {
            agent: "codex",
            resume_id: encode_resume("codex", id),
            title,
            score: mtime_score(&path),
        });
    }
    found
}

fn codex_by_id(id: &str) -> Option<String> {
    let root = codex_home()?;
    codex_thread_names(&root.join("session_index.jsonl")).remove(id)
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
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
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
            Some(DiskTitle {
                agent: "opencode",
                resume_id: encode_resume("opencode", &id),
                title,
                score: updated,
            })
        })
        .collect()
}

fn opencode_by_id(id: &str) -> Option<String> {
    let path = opencode_db()?;
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT title FROM session_v2 WHERE id = ?1",
        [id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .and_then(|t| title_of(&t))
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
        found.push(DiskTitle {
            agent: "pi",
            resume_id: encode_resume("pi", &id),
            title,
            score: mtime_score(&path),
        });
    }
    found
}

fn pi_by_id(id: &str) -> Option<String> {
    let root = pi_sessions_root()?;
    let Ok(dirs) = fs::read_dir(root) else {
        return None;
    };
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
            if let Some((found_id, title)) = pi_file_title(&path)
                && found_id == id
            {
                return Some(title);
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
                name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(title_of);
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
    fn resume_ids_round_trip_with_known_agents() {
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
        writeln!(
            file,
            r#"{{"type":"session_info","id":"a","name":"first"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"session_info","id":"b","name":"refactor auth"}}"#
        )
        .unwrap();
        let _guard = HomeOverride::set(&dir);
        let found = pi_discover(&cwd);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "refactor auth");
        assert_eq!(found[0].resume_id, "pi:sess-1");
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
