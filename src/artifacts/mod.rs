//! Agent artifacts: snapshots an agent publishes under `<project>/.kumu`.
//!
//! A published artifact is a directory with `artifact.json` metadata and one
//! entry file (`index.md`, `index.html`, or `index.txt`). The disk layout is
//! the source of truth: the UI scans it, and no database table is involved.
//!
//! The CLI is the only writer. It stages a complete directory, then renames it
//! into place so a reader never sees a half-written artifact.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

mod serve;
pub use serve::ArtifactServer;

/// Project-local directory that owns every artifact and future Kumu settings.
pub const KUMU_DIR: &str = ".kumu";
/// Directory below [`KUMU_DIR`] holding one directory per artifact.
pub const ARTIFACTS_DIR: &str = "artifacts";
/// Metadata file inside every artifact directory.
pub const META_FILE: &str = "artifact.json";

/// Largest single file an artifact may contain.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Largest total payload of one artifact.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Most files one artifact may contain.
pub const MAX_FILES: usize = 500;

#[derive(Debug)]
pub struct ArtifactError(pub String);

impl ArtifactError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArtifactError {}

impl From<std::io::Error> for ArtifactError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Markdown,
    Html,
    Text,
}

impl ArtifactKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Some(Self::Markdown),
            "html" => Some(Self::Html),
            "text" | "txt" => Some(Self::Text),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "md" | "markdown" | "mdown" | "mkd" => Some(Self::Markdown),
            "html" | "htm" => Some(Self::Html),
            "txt" | "text" => Some(Self::Text),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Text => "txt",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Text => "text",
        }
    }

    /// The kind of a document whose extension says nothing, used for stdin.
    pub fn sniff(bytes: &[u8]) -> Self {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let trimmed = head.trim_start_matches('\u{feff}').trim_start();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("<!doctype") || lower.starts_with("<html") {
            Self::Html
        } else {
            Self::Markdown
        }
    }
}

/// The session that published an artifact.
///
/// `key` is the grouping key the drawer uses: a resumable agent conversation
/// is stable across restarts, while a pane-only fallback keeps artifacts from
/// one terminal together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    #[serde(default = "unattached_key")]
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

fn unattached_key() -> String {
    UNATTACHED.into()
}

pub const UNATTACHED: &str = "unattached";

impl Default for SessionInfo {
    fn default() -> Self {
        Self {
            key: UNATTACHED.into(),
            pane: None,
            agent: None,
            agent_session_id: None,
            title: None,
            tab_name: None,
            cwd: None,
        }
    }
}

impl SessionInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pane: Option<u64>,
        agent: Option<String>,
        agent_session_id: Option<String>,
        title: Option<String>,
        tab_name: Option<String>,
        cwd: Option<String>,
    ) -> Self {
        let key = session_key(pane, agent.as_deref(), agent_session_id.as_deref());
        Self {
            key,
            pane,
            agent,
            agent_session_id,
            title,
            tab_name,
            cwd,
        }
    }

    #[cfg(test)]
    pub fn unattached() -> Self {
        Self {
            key: UNATTACHED.into(),
            ..Default::default()
        }
    }

    pub fn is_unattached(&self) -> bool {
        self.key == UNATTACHED
    }
}

/// Grouping key, in priority order: agent conversation, pane, unattached.
pub fn session_key(pane: Option<u64>, agent: Option<&str>, session_id: Option<&str>) -> String {
    if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("agent-session:{session_id}");
    }
    if let Some(pane) = pane {
        let agent = agent.map(str::trim).filter(|s| !s.is_empty());
        return match agent {
            Some(agent) => format!("pane:{pane}:{agent}"),
            None => format!("pane:{pane}"),
        };
    }
    UNATTACHED.into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactMeta {
    #[serde(default = "default_version")]
    pub version: u32,
    pub id: String,
    pub title: String,
    pub kind: ArtifactKind,
    /// Entry file path relative to the artifact directory.
    pub entry: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub session: SessionInfo,
}

fn default_version() -> u32 {
    1
}

/// A metadata record plus the directory it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub meta: ArtifactMeta,
    pub dir: PathBuf,
}

impl Artifact {
    pub fn id(&self) -> &str {
        &self.meta.id
    }

    pub fn entry_path(&self) -> PathBuf {
        self.dir.join(&self.meta.entry)
    }
}

/// Where an artifact's content comes from.
#[derive(Clone, Debug)]
pub enum PublishInput {
    File(PathBuf),
    Directory(PathBuf),
    Stdin(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct PublishOptions {
    /// The project root, not the `.kumu` directory.
    pub project_root: PathBuf,
    pub input: PublishInput,
    pub title: Option<String>,
    pub kind: Option<ArtifactKind>,
    /// Entry path when publishing a directory.
    pub entry: Option<String>,
    /// Replace an existing artifact in place, keeping its id and creation time.
    pub replace: Option<String>,
    pub session: SessionInfo,
    pub tags: Vec<String>,
    /// Follow symlinks that stay inside the published directory.
    pub follow_symlinks: bool,
}

/// Resolve the project root a CLI invocation should publish into.
///
/// `--project` wins; otherwise the nearest ancestor containing `.kumu` or
/// `.git` (a file counts, for worktrees); otherwise the working directory.
pub fn resolve_project_root(cwd: &Path, explicit: Option<&Path>) -> Result<PathBuf, ArtifactError> {
    resolve_project_root_with_fallback(cwd, explicit).map(|(root, _)| root)
}

/// Like [`resolve_project_root`], also returning whether resolution fell back
/// to `cwd` because neither `.kumu` nor `.git` was found in any ancestor.
pub fn resolve_project_root_with_fallback(
    cwd: &Path,
    explicit: Option<&Path>,
) -> Result<(PathBuf, bool), ArtifactError> {
    if let Some(explicit) = explicit {
        let root = fs::canonicalize(explicit).map_err(|error| {
            ArtifactError::new(format!(
                "project directory {} is not readable: {error}",
                explicit.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ArtifactError::new(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        return Ok((root, false));
    }
    let cwd = fs::canonicalize(cwd)?;
    for dir in cwd.ancestors() {
        if dir.join(KUMU_DIR).is_dir() || dir.join(".git").exists() {
            return Ok((dir.to_path_buf(), false));
        }
    }
    Ok((cwd, true))
}

pub fn kumu_dir(project_root: &Path) -> PathBuf {
    project_root.join(KUMU_DIR)
}

pub fn artifacts_dir(project_root: &Path) -> PathBuf {
    kumu_dir(project_root).join(ARTIFACTS_DIR)
}

/// Publish a snapshot and return its metadata.
pub fn publish(options: PublishOptions) -> Result<Artifact, ArtifactError> {
    let root = fs::canonicalize(&options.project_root).map_err(|error| {
        ArtifactError::new(format!(
            "project directory {} is not readable: {error}",
            options.project_root.display()
        ))
    })?;
    let store = artifacts_dir(&root);
    fs::create_dir_all(&store)?;
    ensure_gitignore(&root)?;

    let replacing = options.replace.as_deref().map(|id| {
        validate_id(id)?;
        load_meta(&store.join(id)).ok_or_else(|| {
            ArtifactError::new(format!(
                "artifact {id} does not exist in {}",
                store.display()
            ))
        })
    });
    let previous = match replacing {
        Some(meta) => Some(meta?),
        None => None,
    };

    let id = previous
        .as_ref()
        .map(|meta| meta.id.clone())
        .unwrap_or_else(|| unique_id(&store));

    let staging = store.join(format!(".tmp-{id}-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;

    let staged = match stage_input(&options, &staging) {
        Ok(staged) => staged,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let kind = options.kind.unwrap_or(staged.kind);
    // A file or stdin is stored as `index.<extension>`. A directory keeps its
    // own layout so relative assets next to the entry still resolve.
    let mut entry = staged.entry.clone();
    if kind != staged.kind && staged.rename_by_kind {
        let from = staging.join(&staged.entry);
        let to = staging.join(format!("index.{}", kind.extension()));
        if from != to {
            fs::rename(&from, &to)?;
            remove_empty_parents(&from, &staging);
        }
        entry = format!("index.{}", kind.extension());
    }
    let entry_path = staging.join(&entry);
    let title = options
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .or_else(|| infer_title(&entry_path, kind))
        .or_else(|| staged.source_name.and_then(|name| file_stem_name(&name)))
        .unwrap_or_else(|| "Untitled".to_string());

    let now = now_rfc3339();
    let session = match &previous {
        Some(meta) if !meta.session.is_unattached() => meta.session.clone(),
        _ => options.session.clone(),
    };
    let meta = ArtifactMeta {
        version: 1,
        id: id.clone(),
        title,
        kind,
        entry,
        created_at: previous
            .as_ref()
            .map(|meta| meta.created_at.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
        source: Some(staged.source),
        tags: options.tags,
        session,
    };

    if let Err(error) = write_meta(&staging, &meta) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let target = store.join(&id);
    if let Err(error) = install(&staging, &target, previous.is_some()) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    Ok(Artifact { meta, dir: target })
}

/// Every readable artifact in a project, newest first.
pub fn scan(project_root: &Path) -> Vec<Artifact> {
    let store = artifacts_dir(project_root);
    let Ok(entries) = fs::read_dir(&store) else {
        return Vec::new();
    };
    let mut artifacts: Vec<Artifact> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|ty| ty.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.starts_with('.') || name.contains(".tmp-") || name.contains(".bak-") {
                return None;
            }
            load_artifact(&entry.path())
        })
        .collect();
    artifacts.sort_by(|a, b| {
        b.meta
            .updated_at
            .cmp(&a.meta.updated_at)
            .then_with(|| b.meta.id.cmp(&a.meta.id))
    });
    artifacts
}

pub fn find(project_root: &Path, id: &str) -> Option<Artifact> {
    validate_id(id).ok()?;
    load_artifact(&artifacts_dir(project_root).join(id))
}

/// Move one artifact directory to the macOS Trash.
pub fn move_to_trash(project_root: &Path, id: &str) -> Result<PathBuf, ArtifactError> {
    validate_id(id)?;
    let dir = artifacts_dir(project_root).join(id);
    if !dir.is_dir() {
        return Err(ArtifactError::new(format!("artifact {id} does not exist")));
    }
    trash_path(&dir)
}

#[cfg(target_os = "macos")]
fn trash_path(path: &Path) -> Result<PathBuf, ArtifactError> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let manager = NSFileManager::defaultManager();
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let mut resulting: Option<objc2::rc::Retained<NSURL>> = None;
    manager
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
        .map_err(|error| {
            ArtifactError::new(format!("could not move artifact to Trash: {error}"))
        })?;
    Ok(resulting.and_then(|url| url.path()).map_or_else(
        || path.to_path_buf(),
        |path| PathBuf::from(path.to_string()),
    ))
}

#[cfg(not(target_os = "macos"))]
fn trash_path(path: &Path) -> Result<PathBuf, ArtifactError> {
    fs::remove_dir_all(path)?;
    Ok(path.to_path_buf())
}

struct Staged {
    entry: String,
    kind: ArtifactKind,
    source: String,
    source_name: Option<String>,
    /// Whether the entry lives at `index.<ext>` and may be renamed when an
    /// explicit `--kind` disagrees with the source extension.
    rename_by_kind: bool,
}

fn stage_input(options: &PublishOptions, staging: &Path) -> Result<Staged, ArtifactError> {
    match &options.input {
        PublishInput::Stdin(bytes) => {
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(ArtifactError::new(format!(
                    "stdin is {} MB; the limit is {} MB",
                    bytes.len() / (1024 * 1024),
                    MAX_FILE_BYTES / (1024 * 1024)
                )));
            }
            let kind = options.kind.unwrap_or_else(|| ArtifactKind::sniff(bytes));
            let entry = format!("index.{}", kind.extension());
            fs::write(staging.join(&entry), bytes)?;
            Ok(Staged {
                entry,
                kind,
                source: "stdin".into(),
                source_name: None,
                rename_by_kind: true,
            })
        }
        PublishInput::File(source) => {
            let source = fs::canonicalize(source).map_err(|error| {
                ArtifactError::new(format!("{} is not readable: {error}", source.display()))
            })?;
            if !source.is_file() {
                return Err(ArtifactError::new(format!(
                    "{} is not a file",
                    source.display()
                )));
            }
            let size = fs::metadata(&source)?.len();
            if size > MAX_FILE_BYTES {
                return Err(ArtifactError::new(format!(
                    "{} is {} MB; the limit is {} MB",
                    source.display(),
                    size / (1024 * 1024),
                    MAX_FILE_BYTES / (1024 * 1024)
                )));
            }
            let kind = options
                .kind
                .or_else(|| ArtifactKind::from_path(&source))
                .unwrap_or_else(|| {
                    if source.extension().is_none() {
                        sniff_file(&source)
                    } else {
                        ArtifactKind::Text
                    }
                });
            let entry = format!("index.{}", kind.extension());
            fs::copy(&source, staging.join(&entry))?;
            Ok(Staged {
                entry,
                kind,
                source: source.to_string_lossy().into_owned(),
                source_name: source
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                rename_by_kind: true,
            })
        }
        PublishInput::Directory(source) => {
            let source = fs::canonicalize(source).map_err(|error| {
                ArtifactError::new(format!("{} is not readable: {error}", source.display()))
            })?;
            if !source.is_dir() {
                return Err(ArtifactError::new(format!(
                    "{} is not a directory",
                    source.display()
                )));
            }
            let entry = directory_entry(&source, options.entry.as_deref())?;
            let kind = options
                .kind
                .or_else(|| ArtifactKind::from_path(&entry))
                .unwrap_or(ArtifactKind::Text);
            let mut budget = Budget::default();
            let mut visited = HashSet::new();
            copy_dir(
                &source,
                &source,
                staging,
                options.follow_symlinks,
                &mut budget,
                &mut visited,
            )?;
            let entry_name = entry.to_string_lossy().replace('\\', "/");
            if !staging.join(&entry_name).is_file() {
                return Err(ArtifactError::new(format!(
                    "entry {entry_name} was skipped (it may be a symlink outside the directory)"
                )));
            }
            Ok(Staged {
                entry: entry_name,
                kind,
                source: source.to_string_lossy().into_owned(),
                source_name: Some(source.file_name().map_or_else(
                    || "directory".to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )),
                rename_by_kind: false,
            })
        }
    }
}

/// The entry file of a published directory, relative to that directory.
fn directory_entry(source: &Path, explicit: Option<&str>) -> Result<PathBuf, ArtifactError> {
    if let Some(explicit) = explicit {
        let entry = safe_relative(explicit)
            .ok_or_else(|| ArtifactError::new(format!("invalid entry path: {explicit}")))?;
        if !source.join(&entry).is_file() {
            return Err(ArtifactError::new(format!(
                "entry {explicit} does not exist in {}",
                source.display()
            )));
        }
        return Ok(entry);
    }
    for candidate in ["index.html", "index.htm", "index.md", "index.txt"] {
        if source.join(candidate).is_file() {
            return Ok(PathBuf::from(candidate));
        }
    }
    let mut fallback = None;
    if let Ok(entries) = fs::read_dir(source) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if ArtifactKind::from_path(&path).is_some() {
                fallback = Some(PathBuf::from(entry.file_name()));
                break;
            }
        }
    }
    fallback.ok_or_else(|| {
        ArtifactError::new(format!(
            "{} has no index.html, index.md, or other document entry; pass --entry",
            source.display()
        ))
    })
}

#[derive(Default)]
struct Budget {
    files: usize,
    bytes: u64,
}

fn copy_dir(
    root: &Path,
    source: &Path,
    target: &Path,
    follow_symlinks: bool,
    budget: &mut Budget,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), ArtifactError> {
    if !visited.insert(source.to_path_buf()) {
        return Ok(());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let name = entry.file_name();
        let to = target.join(&name);
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            if !follow_symlinks {
                eprintln!("kumu: skipped symlink {}", from.display());
                continue;
            }
            let resolved = fs::canonicalize(&from)?;
            if !resolved.starts_with(root) {
                eprintln!("kumu: skipped symlink outside source {}", from.display());
                continue;
            }
            let resolved_meta = fs::metadata(&resolved)?;
            if resolved_meta.is_dir() {
                fs::create_dir_all(&to)?;
                copy_dir(root, &resolved, &to, follow_symlinks, budget, visited)?;
            } else if resolved_meta.is_file() {
                copy_file(&resolved, &to, budget)?;
            }
            continue;
        }
        if metadata.is_dir() {
            fs::create_dir_all(&to)?;
            copy_dir(root, &from, &to, follow_symlinks, budget, visited)?;
            continue;
        }
        if metadata.is_file() {
            copy_file(&from, &to, budget)?;
        }
    }
    Ok(())
}

fn sniff_file(path: &Path) -> ArtifactKind {
    let Ok(mut file) = fs::File::open(path) else {
        return ArtifactKind::Markdown;
    };
    let mut head = [0u8; 4096];
    let read = file.read(&mut head).unwrap_or(0);
    ArtifactKind::sniff(&head[..read])
}

fn copy_file(from: &Path, to: &Path, budget: &mut Budget) -> Result<(), ArtifactError> {
    let size = fs::metadata(from)?.len();
    if size > MAX_FILE_BYTES {
        return Err(ArtifactError::new(format!(
            "{} is {} MB; the limit is {} MB",
            from.display(),
            size / (1024 * 1024),
            MAX_FILE_BYTES / (1024 * 1024)
        )));
    }
    budget.files += 1;
    budget.bytes = budget.bytes.saturating_add(size);
    if budget.files > MAX_FILES {
        return Err(ArtifactError::new(format!(
            "artifact has more than {MAX_FILES} files"
        )));
    }
    if budget.bytes > MAX_TOTAL_BYTES {
        return Err(ArtifactError::new(format!(
            "artifact is larger than {} MB",
            MAX_TOTAL_BYTES / (1024 * 1024)
        )));
    }
    fs::copy(from, to)?;
    Ok(())
}

fn ensure_gitignore(root: &Path) -> Result<(), ArtifactError> {
    let kumu = kumu_dir(root);
    fs::create_dir_all(&kumu)?;
    let path = kumu.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == "artifacts/") {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("artifacts/\n");
    let mut file = fs::File::create(path)?;
    file.write_all(next.as_bytes())?;
    Ok(())
}

fn write_meta(dir: &Path, meta: &ArtifactMeta) -> Result<(), ArtifactError> {
    let bytes =
        serde_json::to_vec_pretty(meta).map_err(|error| ArtifactError::new(error.to_string()))?;
    let mut file = fs::File::create(dir.join(META_FILE))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn load_artifact(dir: &Path) -> Option<Artifact> {
    let meta = load_meta(dir)?;
    if meta.id.is_empty() || !safe_entry(dir, &meta.entry).is_some_and(|path| path.is_file()) {
        return None;
    }
    Some(Artifact {
        meta,
        dir: dir.to_path_buf(),
    })
}

fn load_meta(dir: &Path) -> Option<ArtifactMeta> {
    let bytes = fs::read(dir.join(META_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn install(staging: &Path, target: &Path, replace: bool) -> Result<(), ArtifactError> {
    if !replace || !target.exists() {
        fs::rename(staging, target)?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if swap_dirs(staging, target).is_ok() {
        // `RENAME_SWAP` leaves the previous directory at `staging`.
        let _ = fs::remove_dir_all(staging);
        return Ok(());
    }
    let backup = target.with_file_name(format!(
        ".bak-{}-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&backup);
    fs::rename(target, &backup)?;
    match fs::rename(staging, target) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, target);
            Err(error.into())
        }
    }
}

#[cfg(target_os = "macos")]
fn swap_dirs(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let a = CString::new(a.as_os_str().as_bytes())?;
    let b = CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: both paths are valid NUL-terminated strings and the call only
    // exchanges the two directory entries.
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_empty_parents(path: &Path, stop: &Path) {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == stop || !dir.starts_with(stop) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        parent = dir.parent();
    }
}

fn unique_id(store: &Path) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    for _ in 0..32 {
        let candidate = format!("{stamp}-{}", random_hex(2));
        if !store.join(&candidate).exists()
            && !store
                .join(format!(".tmp-{candidate}-{}", std::process::id()))
                .exists()
        {
            return candidate;
        }
    }
    format!("{stamp}-{}", std::process::id())
}

fn random_hex(bytes: usize) -> String {
    let mut data = vec![0u8; bytes];
    if fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut data))
        .is_err()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        let seed = nanos ^ std::process::id();
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = ((seed >> (index * 8)) & 0xff) as u8;
        }
    }
    let mut out = String::with_capacity(bytes * 2);
    for byte in data {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn validate_id(id: &str) -> Result<(), ArtifactError> {
    let valid = !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if valid {
        Ok(())
    } else {
        Err(ArtifactError::new(format!("invalid artifact id: {id}")))
    }
}

/// A relative path with no `.`/`..`/root components.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

/// Resolve an entry path inside `dir`, refusing anything that escapes it.
fn safe_entry(dir: &Path, entry: &str) -> Option<PathBuf> {
    let relative = safe_relative(entry)?;
    let root = fs::canonicalize(dir).ok()?;
    let path = fs::canonicalize(dir.join(relative)).ok()?;
    path.starts_with(&root).then_some(path)
}

fn infer_title(entry: &Path, kind: ArtifactKind) -> Option<String> {
    let mut file = fs::File::open(entry).ok()?;
    let mut buffer = vec![0u8; 64 * 1024];
    let read = file.read(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer[..read]);
    let title = match kind {
        ArtifactKind::Markdown => text.lines().find_map(|line| {
            let line = line.trim();
            let heading = line.strip_prefix("# ")?.trim();
            (!heading.is_empty()).then(|| heading.trim_end_matches('#').trim().to_string())
        }),
        ArtifactKind::Html => {
            let lower = text.to_ascii_lowercase();
            let start = lower.find("<title")?;
            let open = start + lower[start..].find('>')? + 1;
            let close = lower[open..].find("</title>")? + open;
            let title = text[open..close]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!title.is_empty()).then_some(title)
        }
        ArtifactKind::Text => text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string),
    }?;
    let title = title.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.chars().take(120).collect())
    }
}

fn file_stem_name(name: &str) -> Option<String> {
    let stem = Path::new(name).file_stem()?.to_string_lossy();
    let stem = stem.trim();
    (!stem.is_empty() && stem != "index").then(|| stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let dir = std::env::temp_dir().join(format!(
                "kumu-artifact-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).expect("create temp project");
            Self(dir)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options(root: &Path, input: PublishInput) -> PublishOptions {
        PublishOptions {
            project_root: root.to_path_buf(),
            input,
            title: None,
            kind: None,
            entry: None,
            replace: None,
            session: SessionInfo::unattached(),
            tags: Vec::new(),
            follow_symlinks: false,
        }
    }

    #[test]
    fn session_keys_prefer_the_agent_conversation() {
        assert!(SessionInfo::default().is_unattached());
        assert_eq!(SessionInfo::default().key, UNATTACHED);
        assert_eq!(
            session_key(Some(7), Some("claude"), Some("abc")),
            "agent-session:abc"
        );
        assert_eq!(session_key(Some(7), Some("claude"), None), "pane:7:claude");
        assert_eq!(session_key(Some(7), None, None), "pane:7");
        assert_eq!(session_key(None, None, None), UNATTACHED);
    }

    #[test]
    fn stdin_sniffs_html_and_markdown() {
        assert_eq!(
            ArtifactKind::sniff(b"  <!doctype html><html></html>"),
            ArtifactKind::Html
        );
        assert_eq!(ArtifactKind::sniff(b"# Plan"), ArtifactKind::Markdown);
    }

    #[test]
    fn publish_writes_metadata_entry_and_gitignore() {
        let project = TempProject::new();
        let source = project.0.join("plan.md");
        fs::write(&source, "# Login redesign\n\nBody").expect("write source");
        let artifact = publish(options(&project.0, PublishInput::File(source))).expect("publish");
        assert_eq!(artifact.meta.title, "Login redesign");
        assert_eq!(artifact.meta.kind, ArtifactKind::Markdown);
        assert_eq!(artifact.meta.entry, "index.md");
        assert!(artifact.entry_path().is_file());
        assert_eq!(
            fs::read_to_string(kumu_dir(&project.0).join(".gitignore")).unwrap(),
            "artifacts/\n"
        );
        let scanned = scan(&project.0);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].id(), artifact.id());
    }

    #[test]
    fn replace_keeps_id_created_at_and_session() {
        let project = TempProject::new();
        let source = project.0.join("plan.md");
        fs::write(&source, "# One").expect("write");
        let original_session = SessionInfo::new(
            Some(7),
            Some("claude".into()),
            Some("sess-1".into()),
            Some("登录页".into()),
            None,
            None,
        );
        let mut first_opts = options(&project.0, PublishInput::File(source.clone()));
        first_opts.session = original_session.clone();
        let first = publish(first_opts).expect("first");
        fs::write(&source, "# Two").expect("write");
        let mut next = options(&project.0, PublishInput::File(source));
        next.replace = Some(first.id().to_string());
        next.session = SessionInfo::new(
            Some(9),
            Some("codex".into()),
            Some("sess-2".into()),
            Some("其它会话".into()),
            None,
            None,
        );
        let replaced = publish(next).expect("replace");
        assert_eq!(replaced.id(), first.id());
        assert_eq!(replaced.meta.created_at, first.meta.created_at);
        assert_eq!(replaced.meta.session, original_session);
        assert_eq!(replaced.meta.title, "Two");
        assert_eq!(scan(&project.0).len(), 1);
    }

    #[test]
    fn directory_publish_skips_symlinks_by_default() {
        let project = TempProject::new();
        let source = project.0.join("site");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("index.html"), "<title>Site</title>").unwrap();
        fs::write(project.0.join("secret.md"), "outside").unwrap();
        std::os::unix::fs::symlink(project.0.join("secret.md"), source.join("linked.md")).unwrap();
        let artifact =
            publish(options(&project.0, PublishInput::Directory(source))).expect("publish");
        assert_eq!(artifact.meta.kind, ArtifactKind::Html);
        assert!(artifact.dir.join("index.html").is_file());
        assert!(!artifact.dir.join("linked.md").exists());
    }

    #[test]
    fn explicit_kind_keeps_a_directory_entry_in_place() {
        let project = TempProject::new();
        let source = project.0.join("site");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::create_dir_all(source.join("assets")).unwrap();
        fs::write(source.join("sub/page.html"), "<title>Page</title>").unwrap();
        fs::write(source.join("assets/style.css"), "body{}").unwrap();
        let mut options = options(&project.0, PublishInput::Directory(source));
        options.entry = Some("sub/page.html".into());
        options.kind = Some(ArtifactKind::Markdown);
        let artifact = publish(options).expect("publish");
        assert_eq!(artifact.meta.kind, ArtifactKind::Markdown);
        assert_eq!(artifact.meta.entry, "sub/page.html");
        assert!(artifact.dir.join("sub/page.html").is_file());
        assert!(artifact.dir.join("assets/style.css").is_file());
    }

    #[test]
    fn scan_ignores_staging_and_broken_metadata() {
        let project = TempProject::new();
        let store = artifacts_dir(&project.0);
        fs::create_dir_all(store.join(".tmp-broken-1")).unwrap();
        fs::create_dir_all(store.join("broken")).unwrap();
        fs::write(store.join("broken").join(META_FILE), "{not json").unwrap();
        assert!(scan(&project.0).is_empty());
    }

    #[test]
    fn entry_outside_the_artifact_is_rejected() {
        let project = TempProject::new();
        let source = project.0.join("plan.md");
        fs::write(&source, "# Plan").unwrap();
        let artifact = publish(options(&project.0, PublishInput::File(source))).unwrap();
        assert!(safe_entry(&artifact.dir, "../outside.md").is_none());
        assert!(safe_entry(&artifact.dir, &artifact.meta.entry).is_some());
    }

    #[test]
    fn directory_publish_enforces_file_count_limit() {
        let project = TempProject::new();
        let source = project.0.join("many");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("index.html"), "<title>Many</title>").unwrap();
        for index in 0..MAX_FILES {
            fs::write(source.join(format!("f{index}.txt")), "x").unwrap();
        }
        let error = publish(options(&project.0, PublishInput::Directory(source)))
            .expect_err("must reject directories exceeding MAX_FILES");
        assert!(error.to_string().contains(&MAX_FILES.to_string()));
    }

    #[test]
    fn project_root_walks_up_to_git_or_kumu() {
        let project = TempProject::new();
        let nested = project.0.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        // Worktree `.git` is a file, not a directory.
        fs::write(
            project.0.join(".git"),
            "gitdir: /tmp/main/.git/worktrees/wt\n",
        )
        .unwrap();
        assert_eq!(
            resolve_project_root_with_fallback(&nested, None).unwrap(),
            (fs::canonicalize(&project.0).unwrap(), false)
        );
        // An explicit project root is authoritative even without `.git`/`.kumu`.
        assert_eq!(
            resolve_project_root_with_fallback(&nested, Some(&nested)).unwrap(),
            (fs::canonicalize(&nested).unwrap(), false)
        );
    }
}
