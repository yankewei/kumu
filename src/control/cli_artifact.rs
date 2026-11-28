//! `kumu artifact ...`: the CLI an agent calls to publish and open artifacts.
//!
//! The command writes the project-local `.kumu/artifacts` layout itself, then
//! tells a running Kumu over the control socket. When no Kumu is running the
//! files still land on disk and the UI picks them up when the project opens.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use super::cli::{call, caller_pane, print_json, runtime_error, usage_error};
use super::protocol::{ArtifactNotifyResult, PaneSummary, Request};
use crate::artifacts::{self, Artifact, ArtifactKind, PublishInput, PublishOptions, SessionInfo};

const USAGE: &str = "\
usage:
  kumu artifact publish <FILE|-> [--title T] [--kind auto|markdown|html|text]
      [--entry FILE] [--replace ID] [--open] [--no-notify]
      [--project DIR] [--agent ID] [--session ID] [--tag T]...
      [--follow-symlinks] [--json]
  kumu artifact list [--json] [--limit N] [--session KEY] [--project DIR]
  kumu artifact path <ID> [--project DIR]
  kumu artifact open <ID|PATH> [--project DIR] [--json]
  kumu artifact remove <ID> [--project DIR] [--force]

publish copies a Markdown/HTML/text file (or a directory with --entry) into
<project>/.kumu/artifacts/<id>/ and records the publishing pane, agent and
conversation. A running Kumu refreshes its Artifacts drawer; --open also opens
the reading sheet. `publish -` reads the document from stdin. `--replace`
updates an existing artifact in place. `remove` moves it to the Trash; a
non-interactive caller must pass --force.";

pub(super) fn artifact_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("publish") => publish(&args[1..]),
        Some("list") => list(&args[1..]),
        Some("path") => path(&args[1..]),
        Some("open") => open(&args[1..]),
        Some("remove") | Some("rm") => remove(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            println!("{USAGE}");
            0
        }
        Some(other) => usage_error(&format!("unknown artifact command {other}")),
    }
}

fn publish(args: &[String]) -> i32 {
    let mut title = None;
    let mut kind = None;
    let mut entry = None;
    let mut replace = None;
    let mut open_after = false;
    let mut notify = true;
    let mut project = None;
    let mut agent = None;
    let mut session = None;
    let mut tags = Vec::new();
    let mut follow_symlinks = false;
    let mut json = false;
    let mut source: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--title" | "-t" => match iter.next() {
                Some(value) => title = Some(value.clone()),
                None => return usage_error("--title needs a value"),
            },
            "--kind" | "-k" => match iter.next().map(String::as_str) {
                Some(value) if value.eq_ignore_ascii_case("auto") => kind = None,
                Some(value) => match ArtifactKind::parse(value) {
                    Some(parsed) => kind = Some(parsed),
                    None => return usage_error("--kind must be auto, markdown, html, or text"),
                },
                None => return usage_error("--kind must be auto, markdown, html, or text"),
            },
            "--entry" => match iter.next() {
                Some(value) => entry = Some(value.clone()),
                None => return usage_error("--entry needs a path"),
            },
            "--replace" => match iter.next() {
                Some(value) => replace = Some(value.clone()),
                None => return usage_error("--replace needs an artifact id"),
            },
            "--open" => open_after = true,
            "--no-notify" => notify = false,
            "--project" => match iter.next() {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return usage_error("--project needs a directory"),
            },
            "--agent" => match iter.next() {
                Some(value) => agent = Some(value.clone()),
                None => return usage_error("--agent needs an id"),
            },
            "--session" => match iter.next() {
                Some(value) => session = Some(value.clone()),
                None => return usage_error("--session needs an id"),
            },
            "--tag" => match iter.next() {
                Some(value) => tags.push(value.clone()),
                None => return usage_error("--tag needs a value"),
            },
            "--follow-symlinks" => follow_symlinks = true,
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') && other != "-" => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if source.is_none() => source = Some(other.to_string()),
            _ => return usage_error("publish takes one file, directory, or -"),
        }
    }
    let Some(source) = source else {
        return usage_error("name the file to publish, or - for stdin");
    };

    let pane = caller_pane().and_then(control_pane);
    let explicit_root = pane
        .as_ref()
        .map(|pane| pane.project_path.as_str())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or(project);
    let project_root = match explicit_root {
        // A pane's project (or `--project`) is authoritative: do not walk up
        // and publish into an enclosing repository.
        Some(root) => match artifacts::resolve_project_root(&root, Some(&root)) {
            Ok(root) => root,
            Err(error) => return runtime_error(&error.to_string()),
        },
        None => {
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(error) => {
                    return runtime_error(&format!("cannot read the working directory: {error}"));
                }
            };
            match artifacts::resolve_project_root_with_fallback(&cwd, None) {
                Ok((root, true)) => {
                    eprintln!("kumu: 建议在项目根目录发布，或用 --project 指定");
                    root
                }
                Ok((root, false)) => root,
                Err(error) => return runtime_error(&error.to_string()),
            }
        }
    };

    let session = match pane {
        // A pane already running an agent keeps its pane fallback until the
        // agent reports a conversation id. A plain shell pane publishes as
        // unattached: the user ran the CLI by hand, not from an agent.
        Some(pane) if pane.agent.is_some() => {
            let PaneSummary {
                id,
                agent,
                agent_session_id,
                title,
                tab_name,
                cwd,
                ..
            } = pane;
            SessionInfo::new(
                Some(id),
                agent,
                agent_session_id,
                Some(title),
                (!tab_name.is_empty()).then_some(tab_name),
                (!cwd.is_empty()).then_some(cwd),
            )
        }
        _ => SessionInfo::new(
            None,
            agent,
            session,
            None,
            None,
            std::env::current_dir()
                .ok()
                .map(|cwd| cwd.to_string_lossy().into_owned()),
        ),
    };

    let input = if source == "-" {
        let mut bytes = Vec::new();
        if let Err(error) = std::io::stdin().read_to_end(&mut bytes) {
            return runtime_error(&format!("cannot read stdin: {error}"));
        }
        if bytes.is_empty() {
            return runtime_error("stdin is empty");
        }
        PublishInput::Stdin(bytes)
    } else {
        let path = PathBuf::from(&source);
        if path.is_dir() {
            PublishInput::Directory(path)
        } else {
            PublishInput::File(path)
        }
    };

    let published = match artifacts::publish(PublishOptions {
        project_root: project_root.clone(),
        input,
        title,
        kind,
        entry,
        replace,
        session,
        tags,
        follow_symlinks,
    }) {
        Ok(artifact) => artifact,
        Err(error) => return runtime_error(&error.to_string()),
    };

    if notify {
        let _ = notify_app(
            Request::ArtifactPublished {
                from: caller_pane(),
                project_path: project_root.to_string_lossy().into_owned(),
                artifact: published.id().to_string(),
            },
            false,
        );
    }
    if open_after
        && let Err(error) = notify_app(
            Request::ArtifactOpen {
                from: caller_pane(),
                project_path: project_root.to_string_lossy().into_owned(),
                artifact: published.id().to_string(),
            },
            true,
        )
    {
        eprintln!("kumu: published, but could not open it: {error}");
    }

    if json {
        return print_json(&artifact_json(&published));
    }
    println!(
        "artifact {} · {} · {}",
        published.id(),
        published.meta.title,
        published.meta.kind.as_str()
    );
    println!("{}", published.entry_path().display());
    0
}

fn list(args: &[String]) -> i32 {
    let mut json = false;
    let mut limit = None;
    let mut session = None;
    let mut project = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--limit" | "-n" => match iter.next().and_then(|value| value.parse::<usize>().ok()) {
                Some(value) => limit = Some(value),
                None => return usage_error("--limit needs a number"),
            },
            "--session" => match iter.next() {
                Some(value) => session = Some(value.clone()),
                None => return usage_error("--session needs a key"),
            },
            "--project" => match iter.next() {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return usage_error("--project needs a directory"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => return usage_error(&format!("unknown option {other}")),
        }
    }
    let root = match resolve_root(project.as_deref()) {
        Ok(root) => root,
        Err(error) => return runtime_error(&error),
    };
    let mut found = artifacts::scan(&root);
    if let Some(session) = session {
        found.retain(|artifact| artifact.meta.session.key == session);
    }
    if let Some(limit) = limit {
        found.truncate(limit);
    }
    if json {
        let rows: Vec<_> = found.iter().map(artifact_json).collect();
        return print_json(&rows);
    }
    if found.is_empty() {
        println!(
            "no artifacts in {}",
            artifacts::artifacts_dir(&root).display()
        );
        return 0;
    }
    for artifact in found {
        let session = artifact
            .meta
            .session
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .map(|title| format!(" · {title}"))
            .unwrap_or_default();
        println!(
            "{}  {:<8}  {}{}",
            artifact.id(),
            artifact.meta.kind.as_str(),
            artifact.meta.title,
            session
        );
    }
    0
}

fn path(args: &[String]) -> i32 {
    let mut project = None;
    let mut id = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project" => match iter.next() {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return usage_error("--project needs a directory"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if id.is_none() => id = Some(other.to_string()),
            _ => return usage_error("path takes one artifact id"),
        }
    }
    let Some(id) = id else {
        return usage_error("name the artifact id");
    };
    let root = match resolve_root(project.as_deref()) {
        Ok(root) => root,
        Err(error) => return runtime_error(&error),
    };
    match artifacts::find(&root, &id) {
        Some(artifact) => {
            println!("{}", artifact.entry_path().display());
            0
        }
        None => runtime_error(&format!(
            "artifact {id} does not exist in {}",
            root.display()
        )),
    }
}

fn open(args: &[String]) -> i32 {
    let mut project = None;
    let mut json = false;
    let mut target = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project" => match iter.next() {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return usage_error("--project needs a directory"),
            },
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            _ => return usage_error("open takes one artifact id or path"),
        }
    }
    let Some(target) = target else {
        return usage_error("name the artifact to open");
    };
    let (root, id) = match artifact_from_path(&target) {
        Some((path_root, path_id)) if project.is_none() => (path_root, path_id),
        _ => {
            let root = match resolve_root(project.as_deref()) {
                Ok(root) => root,
                Err(error) => return runtime_error(&error),
            };
            match artifacts::find(&root, &target) {
                Some(artifact) => (root, artifact.id().to_string()),
                None => {
                    return runtime_error(&format!(
                        "artifact {target} does not exist in {}",
                        root.display()
                    ));
                }
            }
        }
    };
    match notify_app(
        Request::ArtifactOpen {
            from: caller_pane(),
            project_path: root.to_string_lossy().into_owned(),
            artifact: id.clone(),
        },
        true,
    ) {
        Ok(result) => {
            if json {
                print_json(&result)
            } else {
                println!("opened {id}");
                0
            }
        }
        Err(error) => runtime_error(&error),
    }
}

fn remove(args: &[String]) -> i32 {
    let mut project = None;
    let mut force = false;
    let mut id = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project" => match iter.next() {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return usage_error("--project needs a directory"),
            },
            "--force" | "-f" => force = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if id.is_none() => id = Some(other.to_string()),
            _ => return usage_error("remove takes one artifact id"),
        }
    }
    let Some(id) = id else {
        return usage_error("name the artifact id");
    };
    let root = match resolve_root(project.as_deref()) {
        Ok(root) => root,
        Err(error) => return runtime_error(&error),
    };
    let Some(artifact) = artifacts::find(&root, &id) else {
        return runtime_error(&format!(
            "artifact {id} does not exist in {}",
            root.display()
        ));
    };
    if !force {
        if !std::io::stdin().is_terminal() {
            return runtime_error("refusing to remove without --force in a non-interactive shell");
        }
        eprint!(
            "Move artifact {} ({}) to the Trash? [y/N] ",
            id, artifact.meta.title
        );
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return runtime_error("could not read the confirmation");
        }
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("cancelled");
            return 1;
        }
    }
    match artifacts::move_to_trash(&root, &id) {
        Ok(path) => {
            println!("moved to Trash: {}", path.display());
            0
        }
        Err(error) => runtime_error(&error.to_string()),
    }
}

pub(super) fn artifact_from_path(target: &str) -> Option<(PathBuf, String)> {
    let path = std::fs::canonicalize(target).ok()?;
    let start = if path.is_dir() {
        path.as_path()
    } else {
        path.parent()?
    };
    for dir in start.ancestors() {
        if !dir.join(artifacts::META_FILE).is_file() {
            continue;
        }
        let id = dir.file_name()?.to_str()?.to_string();
        let artifacts_dir = dir.parent()?;
        if artifacts_dir.file_name()?.to_str()? != artifacts::ARTIFACTS_DIR {
            continue;
        }
        let kumu_dir = artifacts_dir.parent()?;
        if kumu_dir.file_name()?.to_str()? != artifacts::KUMU_DIR {
            continue;
        }
        let root = kumu_dir.parent()?.to_path_buf();
        return Some((root, id));
    }
    None
}

pub(super) fn resolve_root(project: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(pane) = caller_pane().and_then(control_pane)
        && project.is_none()
        && !pane.project_path.is_empty()
    {
        let root = Path::new(&pane.project_path);
        return artifacts::resolve_project_root(root, Some(root))
            .map_err(|error| error.to_string());
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    artifacts::resolve_project_root(&cwd, project).map_err(|error| error.to_string())
}

pub(super) fn control_pane(pane: u64) -> Option<PaneSummary> {
    let value = call(&Request::Panes).ok()?;
    let panes: Vec<PaneSummary> = serde_json::from_value(value.get("panes")?.clone()).ok()?;
    panes.into_iter().find(|candidate| candidate.id == pane)
}

pub(super) fn notify_app(request: Request, required: bool) -> Result<ArtifactNotifyResult, String> {
    match call(&request) {
        Ok(value) => serde_json::from_value(value).map_err(|error| error.to_string()),
        Err(error) if !required && error.contains("cannot reach kumu") => {
            Ok(ArtifactNotifyResult::default())
        }
        Err(error) => Err(error),
    }
}

pub(super) fn artifact_json(artifact: &Artifact) -> serde_json::Value {
    json!({
        "id": artifact.id(),
        "title": artifact.meta.title,
        "kind": artifact.meta.kind.as_str(),
        "entry": artifact.entry_path(),
        "dir": artifact.dir,
        "createdAt": artifact.meta.created_at,
        "updatedAt": artifact.meta.updated_at,
        "source": artifact.meta.source,
        "tags": artifact.meta.tags,
        "session": artifact.meta.session,
    })
}
