//! Pure domain parser and local CLI fetcher for importing external issues
//! (GitHub Issues, GitLab Issues, Jira Tickets) into Kumu's TODO board.
//!
//! Zero UI framework dependencies: parsing is pure and deterministic, and
//! fetching delegates to the user's already-authenticated local CLIs (`gh`,
//! `glab`, `twg`, `jira`) with bounded timeouts.

use std::fmt;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const CLI_TIMEOUT: Duration = Duration::from_secs(15);
const CLI_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerKind {
    GitHub,
    GitLab,
    Jira,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackerRef {
    GitHubIssue {
        host: Option<String>,
        repo: Option<String>,
        number: u64,
    },
    GitLabIssue {
        host: Option<String>,
        project_path: Option<String>,
        number: u64,
    },
    JiraTicket {
        host: Option<String>,
        key: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedTask {
    pub kind: TrackerKind,
    pub source_ref: String,
    pub source_url: String,
    pub title: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackerError {
    InvalidInput(String),
    UnsupportedRemote(String),
    CliNotFound {
        cli: &'static str,
        hint: &'static str,
    },
    CliFailed {
        cli: &'static str,
        detail: String,
    },
    Timeout {
        cli: &'static str,
        secs: u64,
    },
    ParseFailed {
        cli: &'static str,
        detail: String,
    },
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) | Self::UnsupportedRemote(msg) => f.write_str(msg),
            Self::CliNotFound { cli, hint } => write!(f, "未检测到 {cli} 命令：{hint}"),
            Self::CliFailed { cli, detail } => {
                if detail.is_empty() {
                    write!(f, "调用 {cli} 获取任务失败")
                } else {
                    write!(f, "{cli} 执行失败：{detail}")
                }
            }
            Self::Timeout { cli, secs } => {
                write!(f, "调用 {cli} 超时（{secs} 秒），请检查网络或认证状态")
            }
            Self::ParseFailed { cli, detail } => write!(f, "解析 {cli} 返回结果失败：{detail}"),
        }
    }
}

impl std::error::Error for TrackerError {}

/// Returns true when `input` is an HTTP(S) URL pointing at a supported
/// GitHub Issue, GitLab Issue, or Jira Ticket.
pub fn looks_like_tracker_url(input: &str) -> bool {
    let trimmed = input.trim();
    (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && parse_tracker_ref(trimmed, None).is_ok()
}

/// Parse a user-supplied URL or shorthand (`#40`, `PROJ-123`) into a typed
/// [`TrackerRef`]. Shorthand `#N` uses `origin_remote` to infer GitHub vs GitLab.
pub fn parse_tracker_ref(
    input: &str,
    origin_remote: Option<&str>,
) -> Result<TrackerRef, TrackerError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(TrackerError::InvalidInput(
            "请输入 GitHub / GitLab / Jira 链接，或 #40 / PROJ-123".into(),
        ));
    }

    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return parse_tracker_url(trimmed);
    }

    if let Some(rest) = trimmed.strip_prefix('#') {
        let number =
            rest.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                TrackerError::InvalidInput(format!("无效的 Issue 编号：{trimmed}"))
            })?;
        return infer_issue_from_remote(number, origin_remote);
    }

    if is_jira_key(trimmed) {
        return Ok(TrackerRef::JiraTicket {
            host: None,
            key: trimmed.to_string(),
        });
    }

    Err(TrackerError::InvalidInput(
        "无法识别的任务链接或编号，请粘贴 GitHub / GitLab / Jira 链接，或输入 #40 / PROJ-123"
            .into(),
    ))
}

fn parse_tracker_url(raw: &str) -> Result<TrackerRef, TrackerError> {
    let (host, path, query) = split_url(raw)
        .ok_or_else(|| TrackerError::InvalidInput(format!("无法解析链接格式：{raw}")))?;

    // GitLab Issue URL: https://<host>/<group...>/<repo>/-/issues/<n>
    if let Some((before, after)) = path.split_once("/-/issues/") {
        let project_path = before.trim_matches('/');
        let num_seg = after.split('/').next().unwrap_or("");
        if !project_path.is_empty()
            && project_path.contains('/')
            && let Ok(number) = num_seg.parse::<u64>()
            && number > 0
        {
            return Ok(TrackerRef::GitLabIssue {
                host: Some(host),
                project_path: Some(project_path.to_string()),
                number,
            });
        }
    }

    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // GitHub Issue URL: https://<host>/<owner>/<repo>/issues/<n>
    if segments.len() == 4
        && segments[2] == "issues"
        && let Ok(number) = segments[3].parse::<u64>()
        && number > 0
    {
        return Ok(TrackerRef::GitHubIssue {
            host: Some(host),
            repo: Some(format!("{}/{}", segments[0], segments[1])),
            number,
        });
    }

    // Jira URL: ?selectedIssue=<KEY-123>
    if let Some(q) = query
        && let Some(key) = query_param(&q, "selectedIssue")
        && is_jira_key(&key)
    {
        return Ok(TrackerRef::JiraTicket {
            host: Some(host),
            key: key.to_ascii_uppercase(),
        });
    }

    // Jira URL: /browse/<KEY-123>
    if segments.len() >= 2 && segments[0] == "browse" && is_jira_key(segments[1]) {
        return Ok(TrackerRef::JiraTicket {
            host: Some(host),
            key: segments[1].to_ascii_uppercase(),
        });
    }

    // Jira URL: /jira/software/projects/<PROJ>/issues/<KEY-123> (or .../issues/<KEY-123>)
    if let Some(pos) = segments.iter().position(|s| *s == "issues")
        && let Some(candidate) = segments.get(pos + 1)
        && is_jira_key(candidate)
    {
        return Ok(TrackerRef::JiraTicket {
            host: Some(host),
            key: candidate.to_ascii_uppercase(),
        });
    }

    Err(TrackerError::InvalidInput(
        "暂不支持该链接格式，请粘贴 GitHub Issue、GitLab Issue 或 Jira Ticket 链接".into(),
    ))
}

fn infer_issue_from_remote(
    number: u64,
    origin_remote: Option<&str>,
) -> Result<TrackerRef, TrackerError> {
    let Some(remote) = origin_remote.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(TrackerError::UnsupportedRemote(
            "当前仓库未配置 origin 远程地址，无法推断 #编号 所属平台，请粘贴完整链接".into(),
        ));
    };
    let Some((host, path)) = parse_git_remote(remote) else {
        return Err(TrackerError::UnsupportedRemote(
            "无法解析当前仓库的 origin 远程地址，请粘贴完整 Issue 链接".into(),
        ));
    };
    let lower = host.to_ascii_lowercase();
    if lower.contains("github") {
        let repo = path.contains('/').then_some(path);
        return Ok(TrackerRef::GitHubIssue {
            host: Some(host),
            repo,
            number,
        });
    }
    if lower.contains("gitlab") {
        let project_path = path.contains('/').then_some(path);
        return Ok(TrackerRef::GitLabIssue {
            host: Some(host),
            project_path,
            number,
        });
    }
    Err(TrackerError::UnsupportedRemote(format!(
        "无法从远程地址（{host}）推断 GitHub 或 GitLab，请粘贴完整 Issue 链接"
    )))
}

fn split_url(raw: &str) -> Option<(String, String, Option<String>)> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    let before_frag = rest.split('#').next().unwrap_or(rest);
    let (before_query, query) = match before_frag.split_once('?') {
        Some((head, q)) => (head, Some(q.to_string())),
        None => (before_frag, None),
    };
    let (host, path) = match before_query.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (before_query, "/".to_string()),
    };
    let host = host.trim();
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    Some((host.to_string(), path, query))
}

fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == name && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn parse_git_remote(remote: &str) -> Option<(String, String)> {
    let trimmed = remote.trim();
    let (host, raw_path) = if let Some(rest) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("ssh://"))
    {
        let without_user = rest.split_once('@').map(|(_, after)| after).unwrap_or(rest);
        let (host_port, path) = without_user.split_once('/')?;
        let host = host_port.split(':').next().unwrap_or(host_port);
        (host, path)
    } else if let Some((_, after_at)) = trimmed.split_once('@') {
        let (host, path) = after_at.split_once(':')?;
        (host, path)
    } else {
        return None;
    };

    let host = host.trim();
    let path = raw_path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(raw_path.trim_matches('/'))
        .trim_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

fn is_jira_key(s: &str) -> bool {
    let Some((project, num)) = s.split_once('-') else {
        return false;
    };
    if project.len() < 2 || num.is_empty() {
        return false;
    }
    let mut chars = project.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && num.chars().all(|c| c.is_ascii_digit())
}

/// Fetch an external task via the corresponding local CLI (`gh`, `glab`, `twg`,
/// or `jira`) executed in `repo_dir`.
pub fn fetch_task(input: &str, repo_dir: &Path) -> Result<ImportedTask, TrackerError> {
    let origin_remote = if input.trim().starts_with('#') {
        read_git_origin_remote(repo_dir)
    } else {
        None
    };
    let tracker_ref = parse_tracker_ref(input, origin_remote.as_deref())?;
    match tracker_ref {
        TrackerRef::GitHubIssue { host, repo, number } => {
            fetch_github_issue(host.as_deref(), repo.as_deref(), number, repo_dir)
        }
        TrackerRef::GitLabIssue {
            host,
            project_path,
            number,
        } => fetch_gitlab_issue(host.as_deref(), project_path.as_deref(), number, repo_dir),
        TrackerRef::JiraTicket { host, key } => fetch_jira_ticket(host.as_deref(), &key, repo_dir),
    }
}

fn read_git_origin_remote(repo_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

fn fetch_github_issue(
    host: Option<&str>,
    repo: Option<&str>,
    number: u64,
    repo_dir: &Path,
) -> Result<ImportedTask, TrackerError> {
    let num_str = number.to_string();
    let mut args = vec!["issue".to_string(), "view".to_string(), num_str];
    if let Some(repo) = repo {
        let repo_arg = match host {
            Some(h) if !h.eq_ignore_ascii_case("github.com") => format!("{h}/{repo}"),
            _ => repo.to_string(),
        };
        args.push("--repo".to_string());
        args.push(repo_arg);
    }
    args.push("--json".to_string());
    args.push("number,title,body,url".to_string());

    let stdout = run_cli_with_timeout(
        "gh",
        &args,
        repo_dir,
        &[],
        "请先安装并登录 GitHub CLI：brew install gh && gh auth login",
    )?;
    parse_github_issue_json(&stdout, number)
}

fn parse_github_issue_json(
    stdout: &str,
    fallback_number: u64,
) -> Result<ImportedTask, TrackerError> {
    let value: Value = serde_json::from_str(stdout).map_err(|err| TrackerError::ParseFailed {
        cli: "gh",
        detail: err.to_string(),
    })?;
    let number = value
        .get("number")
        .and_then(Value::as_u64)
        .unwrap_or(fallback_number);
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrackerError::ParseFailed {
            cli: "gh",
            detail: "返回结果缺少 title 字段".into(),
        })?
        .to_string();
    let description = value
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let source_url = value
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrackerError::ParseFailed {
            cli: "gh",
            detail: "返回结果缺少 url 字段".into(),
        })?
        .to_string();
    Ok(ImportedTask {
        kind: TrackerKind::GitHub,
        source_ref: format!("GitHub #{number}"),
        source_url,
        title,
        description,
    })
}

fn fetch_gitlab_issue(
    host: Option<&str>,
    project_path: Option<&str>,
    number: u64,
    repo_dir: &Path,
) -> Result<ImportedTask, TrackerError> {
    let num_str = number.to_string();
    let mut args = vec!["issue".to_string(), "view".to_string(), num_str];
    if let Some(project_path) = project_path {
        args.push("-R".to_string());
        args.push(project_path.to_string());
    }
    args.push("-F".to_string());
    args.push("json".to_string());

    let mut envs = Vec::new();
    if let Some(h) = host
        && !h.eq_ignore_ascii_case("gitlab.com")
    {
        envs.push(("GITLAB_HOST", h));
    }

    let stdout = run_cli_with_timeout(
        "glab",
        &args,
        repo_dir,
        &envs,
        "请先安装并登录 GitLab CLI：brew install glab && glab auth login",
    )?;
    parse_gitlab_issue_json(&stdout, number)
}

fn parse_gitlab_issue_json(
    stdout: &str,
    fallback_number: u64,
) -> Result<ImportedTask, TrackerError> {
    let value: Value = serde_json::from_str(stdout).map_err(|err| TrackerError::ParseFailed {
        cli: "glab",
        detail: err.to_string(),
    })?;
    let iid = value
        .get("iid")
        .and_then(Value::as_u64)
        .unwrap_or(fallback_number);
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrackerError::ParseFailed {
            cli: "glab",
            detail: "返回结果缺少 title 字段".into(),
        })?
        .to_string();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let source_url = value
        .get("web_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrackerError::ParseFailed {
            cli: "glab",
            detail: "返回结果缺少 web_url 字段".into(),
        })?
        .to_string();
    Ok(ImportedTask {
        kind: TrackerKind::GitLab,
        source_ref: format!("GitLab #{iid}"),
        source_url,
        title,
        description,
    })
}

fn fetch_jira_ticket(
    host: Option<&str>,
    key: &str,
    repo_dir: &Path,
) -> Result<ImportedTask, TrackerError> {
    let twg_args = vec![
        "jira".to_string(),
        "workitem".to_string(),
        "get".to_string(),
        "--id".to_string(),
        key.to_string(),
        "-o".to_string(),
        "json".to_string(),
    ];
    match run_cli_with_timeout("twg", &twg_args, repo_dir, &[], "") {
        Ok(stdout) => return parse_jira_json("twg", &stdout, key, host),
        Err(TrackerError::CliNotFound { .. }) => {}
        Err(err) => return Err(err),
    }

    let jira_args = vec![
        "issue".to_string(),
        "view".to_string(),
        key.to_string(),
        "--raw".to_string(),
    ];
    match run_cli_with_timeout("jira", &jira_args, repo_dir, &[], "") {
        Ok(stdout) => parse_jira_json("jira", &stdout, key, host),
        Err(TrackerError::CliNotFound { .. }) => Err(TrackerError::CliNotFound {
            cli: "twg / jira",
            hint: "请先安装并登录 twg 或 jira-cli（brew install ankitpokhrel/jira-cli/jira-cli && jira init）",
        }),
        Err(err) => Err(err),
    }
}

fn parse_jira_json(
    cli: &'static str,
    stdout: &str,
    fallback_key: &str,
    host_hint: Option<&str>,
) -> Result<ImportedTask, TrackerError> {
    let value: Value = serde_json::from_str(stdout).map_err(|err| TrackerError::ParseFailed {
        cli,
        detail: err.to_string(),
    })?;
    let fields = value.get("fields");
    let key = value
        .get("key")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_key)
        .to_string();
    let title = value
        .get("title")
        .or_else(|| value.get("summary"))
        .or_else(|| fields.and_then(|f| f.get("summary")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrackerError::ParseFailed {
            cli,
            detail: "返回结果缺少 summary/title 字段".into(),
        })?
        .to_string();
    let description = value
        .get("description")
        .or_else(|| fields.and_then(|f| f.get("description")))
        .map(extract_jira_description)
        .unwrap_or_default();

    let source_url = value
        .get("url")
        .or_else(|| value.get("webUrl"))
        .or_else(|| value.get("web_url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| host_hint.map(|h| format!("https://{h}/browse/{key}")))
        .or_else(|| {
            value
                .get("self")
                .and_then(Value::as_str)
                .and_then(split_url)
                .map(|(h, _, _)| format!("https://{h}/browse/{key}"))
        })
        .unwrap_or_else(|| format!("jira://{key}"));

    Ok(ImportedTask {
        kind: TrackerKind::Jira,
        source_ref: key,
        source_url,
        title,
        description,
    })
}

/// Extract plain text from either a Jira string description or an Atlassian
/// Document Format (ADF) JSON tree.
fn extract_jira_description(value: &Value) -> String {
    match value {
        Value::String(s) => s.trim().to_string(),
        Value::Object(_) => {
            let mut out = String::new();
            collect_adf_text(value, &mut out);
            out.trim().to_string()
        }
        _ => String::new(),
    }
}

fn collect_adf_text(node: &Value, out: &mut String) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    match node_type {
        "text" => {
            if let Some(t) = node.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
            return;
        }
        "hardBreak" => {
            out.push('\n');
            return;
        }
        "inlineCard" => {
            if let Some(url) = node
                .get("attrs")
                .and_then(|a| a.get("url"))
                .and_then(Value::as_str)
            {
                out.push_str(url);
            }
            return;
        }
        _ => {}
    }

    let is_block = matches!(
        node_type,
        "paragraph" | "heading" | "listItem" | "blockquote" | "codeBlock"
    );
    if is_block && !out.is_empty() && !out.ends_with('\n') && !out.ends_with("- ") {
        out.push('\n');
    }
    if node_type == "listItem" {
        out.push_str("- ");
    }

    if let Some(children) = node.get("content").and_then(Value::as_array) {
        for child in children {
            collect_adf_text(child, out);
        }
    }

    if is_block && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn run_cli_with_timeout(
    cli: &'static str,
    args: &[String],
    repo_dir: &Path,
    envs: &[(&str, &str)],
    install_hint: &'static str,
) -> Result<String, TrackerError> {
    let mut cmd = Command::new(cli);
    cmd.args(args)
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            TrackerError::CliNotFound {
                cli,
                hint: install_hint,
            }
        } else {
            TrackerError::CliFailed {
                cli,
                detail: err.to_string(),
            }
        }
    })?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + CLI_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(TrackerError::Timeout {
                        cli,
                        secs: CLI_TIMEOUT.as_secs(),
                    });
                }
                thread::sleep(CLI_POLL_INTERVAL);
            }
            Err(err) => {
                let _ = child.kill();
                return Err(TrackerError::CliFailed {
                    cli,
                    detail: err.to_string(),
                });
            }
        }
    };

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let detail = trim_cli_error(&stderr);
        return Err(TrackerError::CliFailed { cli, detail });
    }
    Ok(String::from_utf8_lossy(&stdout_bytes).into_owned())
}

fn trim_cli_error(stderr: &str) -> String {
    let cleaned = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ");
    if cleaned.chars().count() > 240 {
        let prefix: String = cleaned.chars().take(240).collect();
        format!("{prefix}…")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_issue_urls_and_enterprise_hosts() {
        assert_eq!(
            parse_tracker_ref(
                "https://github.com/kumu-one/kumu/issues/40?foo=bar#top",
                None
            )
            .unwrap(),
            TrackerRef::GitHubIssue {
                host: Some("github.com".into()),
                repo: Some("kumu-one/kumu".into()),
                number: 40,
            }
        );
        assert_eq!(
            parse_tracker_ref("https://ghe.example.com/team/app/issues/128/", None).unwrap(),
            TrackerRef::GitHubIssue {
                host: Some("ghe.example.com".into()),
                repo: Some("team/app".into()),
                number: 128,
            }
        );
        assert!(looks_like_tracker_url(
            "https://github.com/kumu-one/kumu/issues/40"
        ));
        assert!(!looks_like_tracker_url("#40"));
        assert!(!looks_like_tracker_url("Fix login button"));
    }

    #[test]
    fn parses_gitlab_issue_urls_including_nested_subgroups() {
        assert_eq!(
            parse_tracker_ref(
                "https://gitlab.example.com/org/sub/team/service/-/issues/99#note_1",
                None
            )
            .unwrap(),
            TrackerRef::GitLabIssue {
                host: Some("gitlab.example.com".into()),
                project_path: Some("org/sub/team/service".into()),
                number: 99,
            }
        );
    }

    #[test]
    fn parses_jira_urls_and_shorthand() {
        assert_eq!(
            parse_tracker_ref("https://acme.atlassian.net/browse/PROJ-123", None).unwrap(),
            TrackerRef::JiraTicket {
                host: Some("acme.atlassian.net".into()),
                key: "PROJ-123".into(),
            }
        );
        assert_eq!(
            parse_tracker_ref(
                "https://acme.atlassian.net/jira/software/projects/PROJ/issues/PROJ-456",
                None
            )
            .unwrap(),
            TrackerRef::JiraTicket {
                host: Some("acme.atlassian.net".into()),
                key: "PROJ-456".into(),
            }
        );
        assert_eq!(
            parse_tracker_ref(
                "https://acme.atlassian.net/jira/software/projects/PROJ/boards/7?selectedIssue=ENG-88&sprint=2",
                None
            )
            .unwrap(),
            TrackerRef::JiraTicket {
                host: Some("acme.atlassian.net".into()),
                key: "ENG-88".into(),
            }
        );
        assert_eq!(
            parse_tracker_ref("KUMU-40", None).unwrap(),
            TrackerRef::JiraTicket {
                host: None,
                key: "KUMU-40".into(),
            }
        );
    }

    #[test]
    fn infers_hash_shorthand_from_git_remote() {
        assert_eq!(
            parse_tracker_ref("#40", Some("git@github.com:kumu-one/kumu.git")).unwrap(),
            TrackerRef::GitHubIssue {
                host: Some("github.com".into()),
                repo: Some("kumu-one/kumu".into()),
                number: 40,
            }
        );
        assert_eq!(
            parse_tracker_ref("#15", Some("https://gitlab.corp.dev/a/b/repo.git")).unwrap(),
            TrackerRef::GitLabIssue {
                host: Some("gitlab.corp.dev".into()),
                project_path: Some("a/b/repo".into()),
                number: 15,
            }
        );
        assert!(matches!(
            parse_tracker_ref("#40", Some("https://code.internal.local/team/repo.git")),
            Err(TrackerError::UnsupportedRemote(_))
        ));
        assert!(matches!(
            parse_tracker_ref("#40", None),
            Err(TrackerError::UnsupportedRemote(_))
        ));
    }

    #[test]
    fn parses_github_gitlab_and_jira_adf_json() {
        let gh = parse_github_issue_json(
            r#"{"number":40,"title":"Import external issues","body":"Support GitHub/GitLab/Jira","url":"https://github.com/kumu-one/kumu/issues/40"}"#,
            40,
        )
        .unwrap();
        assert_eq!(gh.kind, TrackerKind::GitHub);
        assert_eq!(gh.source_ref, "GitHub #40");
        assert_eq!(gh.title, "Import external issues");

        let gl = parse_gitlab_issue_json(
            r#"{"iid":7,"title":"Fix pipeline","description":"Details here","web_url":"https://gitlab.com/org/repo/-/issues/7"}"#,
            7,
        )
        .unwrap();
        assert_eq!(gl.kind, TrackerKind::GitLab);
        assert_eq!(gl.source_ref, "GitLab #7");
        assert_eq!(gl.source_url, "https://gitlab.com/org/repo/-/issues/7");

        let jira_adf = r#"{
            "key": "PROJ-123",
            "self": "https://acme.atlassian.net/rest/api/3/issue/10001",
            "fields": {
                "summary": "Migrate auth middleware",
                "description": {
                    "type": "doc",
                    "version": 1,
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [
                                {"type": "text", "text": "Replace legacy token parser."}
                            ]
                        },
                        {
                            "type": "bulletList",
                            "content": [
                                {
                                    "type": "listItem",
                                    "content": [
                                        {"type": "paragraph", "content": [{"type": "text", "text": "Keep backwards compat"}]}
                                    ]
                                }
                            ]
                        }
                    ]
                }
            }
        }"#;
        let jira = parse_jira_json("jira", jira_adf, "PROJ-123", None).unwrap();
        assert_eq!(jira.kind, TrackerKind::Jira);
        assert_eq!(jira.source_ref, "PROJ-123");
        assert_eq!(
            jira.source_url,
            "https://acme.atlassian.net/browse/PROJ-123"
        );
        assert_eq!(jira.title, "Migrate auth middleware");
        assert!(jira.description.contains("Replace legacy token parser."));
        assert!(jira.description.contains("- Keep backwards compat"));
    }
}
