//! Git worktrees and commit history for projects.
//!
//! Kumu splits one repository into several projects without copying it: each
//! worktree is a sibling directory of the repository and a project row of its
//! own. A worktree created elsewhere can be attached to the same project list;
//! removing its row from Kumu leaves the directory alone.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// A worktree created beside the repository it belongs to.
pub struct Worktree {
    /// Branch checked out in the new worktree; this becomes its project name.
    pub name: String,
    pub path: PathBuf,
}

/// A worktree git already knows about, whether Kumu created it or not.
pub struct WorktreeEntry {
    /// Branch checked out there, or the directory name for a detached HEAD.
    pub name: String,
    pub path: PathBuf,
    /// The repository's own checkout, which is the project itself.
    pub main: bool,
}

/// One commit as the history panel shows it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Commit {
    /// Full hash, for copying or a future detail view.
    pub hash: String,
    /// Abbreviated hash shown in the row.
    pub short: String,
    /// `%D` decorations split into individual names (`HEAD -> main`, `tag: v1`,
    /// a remote branch, or a local branch).
    pub refs: Vec<String>,
    pub author: String,
    pub email: String,
    pub date: String,
    pub subject: String,
    /// Commit message body without the subject line.
    pub body: String,
    /// Parent count: more than one means the commit is a merge.
    pub parents: usize,
}

/// The checked-out branch and its commits, newest first.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct History {
    /// Current branch, or a short hash for a detached HEAD. Never empty.
    pub branch: String,
    /// Shared with the panel's list instead of copied, so the rows exist once.
    pub commits: Arc<Vec<Commit>>,
}

/// One file with changes that are not committed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Change {
    /// Path relative to the repository root; for a rename, the new path.
    pub path: String,
    /// The path the file had before a rename or copy.
    pub original: Option<String>,
    /// The status git reports for this side: `M`, `A`, `D`, `R`, `C`, `T`,
    /// `U` for an unmerged file, or `?` for an untracked one.
    pub status: char,
    /// The change is in the index; otherwise it is only in the working tree.
    pub staged: bool,
}

/// The repository's uncommitted changes, in the order git lists them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Changes {
    pub staged: Vec<Change>,
    pub unstaged: Vec<Change>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }

    pub fn len(&self) -> usize {
        self.staged.len() + self.unstaged.len()
    }
}

/// A patch the diff view can parse.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Patch {
    /// Unified patch in git's own format, possibly cut at [`MAX_PATCH_BYTES`].
    pub patch: String,
    /// The patch was cut, so the view has to say that it is incomplete.
    pub truncated: bool,
}

/// A commit that rewrites a generated file can produce a patch far bigger than
/// anyone reads, so the panel stops at this many bytes of patch text.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
/// Upper bound on commits loaded into the Git Log panel at once.
const MAX_HISTORY_COMMITS: &str = "5000";
/// Upper bound on captured stderr from a git subprocess.
const MAX_STDERR_BYTES: u64 = 64 * 1024;

/// Read recent commit history of `dir`'s checked-out branch, bounded by
/// [`MAX_HISTORY_COMMITS`] so huge repositories do not exhaust memory.
pub fn history(dir: &Path) -> Result<History, String> {
    let root = repo_root(dir)?;
    let branch = current_branch(&root);
    // One commit is nine NUL-separated fields. NUL cannot appear inside a
    // commit field, so subjects and bodies never need quoting.
    let output = match git_raw(
        &root,
        &[
            "--no-optional-locks",
            "log",
            "-n",
            MAX_HISTORY_COMMITS,
            "-z",
            "--date=short",
            "--pretty=format:%H%x00%h%x00%D%x00%an%x00%ae%x00%cd%x00%s%x00%P%x00%b",
        ],
    ) {
        Ok(output) => output,
        Err(error) => {
            // An unborn branch has no commits to log. That is an empty
            // history, not a failure to show as an error.
            if git(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
                return Ok(History {
                    branch,
                    commits: Arc::default(),
                });
            }
            return Err(error);
        }
    };
    let mut fields = output.split(|byte| *byte == 0);
    let mut commits = Vec::new();
    while let Some(hash) = fields.next() {
        if hash.is_empty() {
            break;
        }
        let short = text(fields.next());
        let refs = text(fields.next());
        let author = text(fields.next());
        let email = text(fields.next());
        let date = text(fields.next());
        let subject = text(fields.next());
        let parents = text(fields.next());
        let body = text(fields.next());
        commits.push(Commit {
            hash: text(Some(hash)),
            short,
            refs: refs
                .split(", ")
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
            author,
            email,
            date,
            subject,
            body,
            parents: parents.split_whitespace().count(),
        });
    }
    Ok(History {
        branch,
        commits: Arc::new(commits),
    })
}

/// The patch `hash` introduced, cut at [`MAX_PATCH_BYTES`].
///
/// `--format=` leaves the commit message out and `-m --first-parent` gives a
/// merge the diff against the commit it was merged into, which plain `show`
/// prints nothing for; `--no-color` keeps a `color.ui=always` configuration
/// from putting escape codes in the patch.
pub fn commit_diff(dir: &Path, hash: &str) -> Result<Patch, String> {
    if hash.is_empty() || hash.starts_with('-') {
        return Err("无效的提交哈希".to_string());
    }
    let root = repo_root(dir)?;
    let args: Vec<std::ffi::OsString> = [
        "--no-optional-locks",
        "show",
        "--format=",
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-m",
        "--first-parent",
        "--end-of-options",
        hash,
        "--",
    ]
    .into_iter()
    .map(std::ffi::OsString::from)
    .collect();
    read_patch(&root, &args, false)
}

/// Read what the index and the working tree changed.
///
/// `-uall` lists every untracked file instead of folding a new directory into
/// one row, and `--no-optional-locks` keeps a poll from refreshing and writing
/// the index under the git commands the user runs in a terminal.
pub fn changes(dir: &Path) -> Result<Changes, String> {
    let root = repo_root(dir)?;
    let output = git_raw(
        &root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "-uall",
        ],
    )?;
    Ok(parse_status(&output))
}

/// Which slice of uncommitted changes to include in a combined working-tree diff.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WorkingTreeDiffScope {
    /// Both staged and unstaged tracked changes (relative to `HEAD`), plus untracked files.
    #[default]
    All,
    /// Only staged changes (`git diff --cached`).
    Staged,
    /// Only unstaged tracked changes (`git diff`) plus untracked files.
    Unstaged,
}

/// The patch one uncommitted change introduced, cut at [`MAX_PATCH_BYTES`].
pub fn change_diff(dir: &Path, change: &Change) -> Result<Patch, String> {
    let root = repo_root(dir)?;
    let untracked = change.status == '?';
    let mut args: Vec<std::ffi::OsString> = vec![
        "--no-optional-locks".into(),
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
    ];
    if untracked {
        args.push("--no-index".into());
    } else if change.staged {
        args.push("--cached".into());
    }
    args.push("--".into());
    if untracked {
        args.push("/dev/null".into());
        // Pass an absolute path so an untracked file literally named `-` is
        // never treated as stdin by `git diff --no-index`.
        args.push(root.join(&change.path).into_os_string());
    } else {
        if let Some(original) = &change.original {
            args.push(original.into());
        }
        args.push((&change.path).into());
    }
    let mut patch = read_patch(&root, &args, untracked)?;
    if untracked {
        patch.patch = normalize_untracked_patch(&patch.patch, &change.path);
    }
    Ok(patch)
}

/// Combined unified diff of uncommitted changes in `dir` for `scope`, cut at
/// [`MAX_PATCH_BYTES`]. Includes untracked files for [`WorkingTreeDiffScope::All`]
/// and [`WorkingTreeDiffScope::Unstaged`].
pub fn working_tree_diff(dir: &Path, scope: WorkingTreeDiffScope) -> Result<Patch, String> {
    let root = repo_root(dir)?;
    let has_head = git(
        &root,
        &["--no-optional-locks", "rev-parse", "--verify", "--quiet", "HEAD"],
    )
    .is_ok();

    let mut args: Vec<std::ffi::OsString> = vec![
        "--no-optional-locks".into(),
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
    ];
    match scope {
        WorkingTreeDiffScope::All => {
            if has_head {
                args.push("HEAD".into());
            } else {
                args.push("--cached".into());
            }
        }
        WorkingTreeDiffScope::Staged => {
            args.push("--cached".into());
        }
        WorkingTreeDiffScope::Unstaged => {}
    }

    let mut combined = read_patch(&root, &args, false)?;
    if scope == WorkingTreeDiffScope::Staged || combined.truncated {
        return Ok(combined);
    }

    let untracked_files: Vec<Change> = changes(&root)?
        .unstaged
        .into_iter()
        .filter(|change| change.status == '?')
        .collect();
    for file in untracked_files {
        if combined.patch.len() >= MAX_PATCH_BYTES {
            combined.truncated = true;
            break;
        }
        let Ok(file_patch) = change_diff(&root, &file) else {
            continue;
        };
        if file_patch.patch.is_empty() {
            continue;
        }
        if !combined.patch.is_empty() && !combined.patch.ends_with('\n') {
            combined.patch.push('\n');
        }
        let remaining = MAX_PATCH_BYTES.saturating_sub(combined.patch.len());
        if file_patch.patch.len() > remaining {
            combined.truncated = true;
            break;
        }
        combined.patch.push_str(&file_patch.patch);
        if file_patch.truncated {
            combined.truncated = true;
            break;
        }
    }

    Ok(combined)
}

/// Rewrite `git diff --no-index /dev/null /abs/path/to/file` headers so the
/// file shows its repository-relative path (`a/<rel> b/<rel>`) in the diff view.
fn normalize_untracked_patch(raw: &str, rel_path: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (idx, line) in raw.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let ending = &line[trimmed.len()..];
        if idx == 0 && trimmed.starts_with("diff --git ") {
            out.push_str(&format!("diff --git a/{rel_path} b/{rel_path}{ending}"));
        } else if trimmed.starts_with("+++ ") && trimmed != "+++ /dev/null" {
            out.push_str(&format!("+++ b/{rel_path}{ending}"));
        } else {
            out.push_str(line);
        }
    }
    out
}

struct ChildGuard(Option<std::process::Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Read the patch `args` print, stopped at [`MAX_PATCH_BYTES`]. `diff --no-index`
/// exits 1 when it found differences, which `differences_are_ok` accepts as a
/// patch instead of an error.
fn read_patch(
    dir: &Path,
    args: &[std::ffi::OsString],
    differences_are_ok: bool,
) -> Result<Patch, String> {
    let child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法运行 git：{error}"))?;
    let mut guard = ChildGuard(Some(child));

    // stderr is drained on its own thread: a chatty git must not fill the pipe
    // and block while the patch is read.
    let stderr = guard.0.as_mut().and_then(|c| c.stderr.take()).map(drain);
    let mut output = Vec::new();
    if let Some(stdout) = guard.0.as_mut().and_then(|c| c.stdout.take()) {
        stdout
            .take(MAX_PATCH_BYTES as u64 + 1)
            .read_to_end(&mut output)
            .map_err(|error| format!("无法读取 git 输出：{error}"))?;
    }
    // Everything past the cap would be thrown away, so stop git instead of
    // waiting for output nobody reads.
    let truncated = output.len() > MAX_PATCH_BYTES;
    let mut child = guard.0.take().expect("git child");
    if truncated {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| format!("无法等待 git：{error}"))?;
    let stderr = stderr
        .map(|stderr| stderr.join().unwrap_or_default())
        .unwrap_or_default();
    let changed = differences_are_ok && status.code() == Some(1);
    if !truncated && !status.success() && !changed {
        let command = args
            .iter()
            .find_map(|arg| {
                let s = arg.to_str()?;
                (!s.starts_with('-')).then_some(s)
            })
            .unwrap_or("git");
        return Err(git_failure(command, status, &stderr));
    }
    let patch = String::from_utf8_lossy(&output).into_owned();
    Ok(Patch {
        patch: if truncated { trim_patch(patch) } else { patch },
        truncated,
    })
}

/// Split porcelain v2 output into the staged and unstaged lists. Records are
/// NUL-terminated and a rename's old path is the record right after it, which
/// is what lets every path be read as-is instead of git's quoted form.
fn parse_status(output: &[u8]) -> Changes {
    let mut changes = Changes::default();
    let mut records = output.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        let record = String::from_utf8_lossy(record);
        let record = record.as_ref();
        match record.as_bytes().first() {
            // An ordinary change: `1 XY sub mH mI mW hH hI path`.
            Some(b'1') => {
                let mut fields = record.splitn(9, ' ');
                let xy = fields.nth(1).unwrap_or("");
                // sub, mH, mI, mW, hH and hI sit between the status and path.
                let path = fields.nth(6).unwrap_or("");
                push_xy(&mut changes, xy, path, None);
            }
            // A rename or copy: `2 XY sub mH mI mW hH hI Xscore path`, with
            // the original path as the next record.
            Some(b'2') => {
                let mut fields = record.splitn(10, ' ');
                let xy = fields.nth(1).unwrap_or("");
                // sub, mH, mI, mW, hH, hI and Xscore sit before the path.
                let path = fields.nth(7).unwrap_or("");
                let original = records
                    .next()
                    .map(|path| String::from_utf8_lossy(path).into_owned());
                push_xy(&mut changes, xy, path, original.as_deref());
            }
            // An unmerged file: `u XY sub m1 m2 m3 mW h1 h2 h3 path`.
            Some(b'u') => {
                let mut fields = record.splitn(11, ' ');
                let xy = fields.nth(1).unwrap_or("");
                // sub, the three stage modes, mW and the three stage hashes
                // sit before the path.
                let path = fields.nth(8).unwrap_or("");
                push_xy(&mut changes, xy, path, None);
            }
            Some(b'?') => {
                if let Some(path) = record.strip_prefix("? ") {
                    changes.unstaged.push(Change {
                        path: path.to_string(),
                        original: None,
                        status: '?',
                        staged: false,
                    });
                }
            }
            // Ignored entries stay out of the list; `--ignored` was not asked
            // for, so this only guards against a record nobody draws.
            _ => {}
        }
    }
    changes
}

/// Record both sides of a porcelain `XY` pair: `X` is the index and `Y` the
/// working tree, and `.` means that side is unchanged.
fn push_xy(changes: &mut Changes, xy: &str, path: &str, original: Option<&str>) {
    let mut letters = xy.chars();
    let index = letters.next().unwrap_or('.');
    let worktree = letters.next().unwrap_or('.');
    for (status, staged) in [(index, true), (worktree, false)] {
        if status == '.' {
            continue;
        }
        let change = Change {
            path: path.to_string(),
            original: original.map(str::to_string),
            status,
            staged,
        };
        if staged {
            changes.staged.push(change);
        } else {
            changes.unstaged.push(change);
        }
    }
}

/// Read a child's stderr up to [`MAX_STDERR_BYTES`] on its own thread.
fn drain(pipe: ChildStderr) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.take(MAX_STDERR_BYTES).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Cut a capped patch to the start of its last file, so every file the view
/// shows is complete. A single file larger than the cap is cut mid-hunk, which
/// the diff parser accepts as a truncated hunk.
fn trim_patch(patch: String) -> String {
    trim_patch_at(patch, MAX_PATCH_BYTES)
}

fn trim_patch_at(mut patch: String, limit: usize) -> String {
    let limit = prefix_boundary(&patch, limit);
    let cut = patch[..limit]
        .rfind("\ndiff --git ")
        .map_or(limit, |index| index + 1);
    patch.truncate(cut);
    patch
}

/// The largest index at or below `limit` that is on a char boundary of `text`.
fn prefix_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// A cheap fingerprint of what [`history`] would show: the checked-out branch
/// and every branch, tag and remote ref. Reading it costs three small git
/// commands instead of a walk over the whole log, so a panel that polls can
/// keep the history it has until something it draws changes.
pub fn fingerprint(dir: &Path) -> Result<String, String> {
    let root = repo_root(dir)?;
    let branch = current_branch(&root);
    let refs = git(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
            "refs/remotes",
        ],
    )?;
    Ok(format!("{branch}\n{refs}"))
}

fn text(bytes: Option<&[u8]>) -> String {
    bytes
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default()
}

/// The checked-out branch, falling back to a short commit for a detached HEAD
/// and to `HEAD` in a repository with no commits yet.
fn current_branch(root: &Path) -> String {
    if let Ok(branch) = git(root, &["symbolic-ref", "--short", "-q", "HEAD"])
        && !branch.is_empty()
    {
        return branch;
    }
    git(root, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|_| "HEAD".to_string())
}

/// Create `branch` in a directory next to the repository that contains
/// `dir`. The directory is named `<repo>-<branch>`, with any `/` in the
/// branch flattened to `-`. An existing branch is attached rather than
/// recreated.
pub fn create_worktree(dir: &Path, branch: &str) -> Result<Worktree, String> {
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') || branch.contains("@{") {
        return Err(format!("「{branch}」不是有效的分支名"));
    }
    let root = repo_root(dir)?;
    let branch = git(&root, &["check-ref-format", "--branch", branch])
        .map_err(|_| format!("「{branch}」不是有效的分支名"))?;
    if branch.is_empty() || branch.starts_with('-') {
        return Err(format!("「{branch}」不是有效的分支名"));
    }

    let repo_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "无法确定仓库目录名".to_string())?;
    let sibling = root
        .parent()
        .ok_or_else(|| "仓库没有上级目录，无法创建同级 worktree".to_string())?;
    let path = sibling.join(format!("{repo_name}-{}", branch.replace('/', "-")));
    if path.exists() {
        return Err(format!("目录已存在：{}", path.display()));
    }

    let branch_ref = format!("refs/heads/{branch}");
    if git(&root, &["rev-parse", "--verify", "--quiet", &branch_ref]).is_ok() {
        git_raw_os(
            &root,
            &[
                std::ffi::OsStr::new("worktree"),
                std::ffi::OsStr::new("add"),
                std::ffi::OsStr::new("--"),
                path.as_os_str(),
                std::ffi::OsStr::new(&branch),
            ],
        )?;
    } else {
        git_raw_os(
            &root,
            &[
                std::ffi::OsStr::new("worktree"),
                std::ffi::OsStr::new("add"),
                std::ffi::OsStr::new("-b"),
                std::ffi::OsStr::new(&branch),
                std::ffi::OsStr::new("--"),
                path.as_os_str(),
            ],
        )?;
    }

    Ok(Worktree { name: branch, path })
}

/// Current branch checked out in `dir`, or the abbreviated commit hash when
/// `HEAD` is detached. Returns `None` when `dir` is not inside a git working
/// tree.
pub fn repo_branch(dir: &Path) -> Option<String> {
    if let Ok(branch) = git(
        dir,
        &["--no-optional-locks", "symbolic-ref", "--short", "HEAD"],
    ) && !branch.is_empty()
    {
        return Some(branch);
    }
    if let Ok(short) = git(
        dir,
        &["--no-optional-locks", "rev-parse", "--short", "HEAD"],
    ) && !short.is_empty()
    {
        return Some(short);
    }
    if git(
        dir,
        &["--no-optional-locks", "rev-parse", "--is-inside-work-tree"],
    )
    .ok()
    .as_deref()
        == Some("true")
    {
        return Some("HEAD".to_string());
    }
    None
}

/// Every worktree attached to the repository that contains `dir`, including
/// the main checkout. Missing directories (git still lists a worktree until it
/// is pruned) are left out: Kumu cannot open them.
pub fn list_worktrees(dir: &Path) -> Result<Vec<WorktreeEntry>, String> {
    let root = repo_root(dir)?;
    let output = git_raw(&root, &["worktree", "list", "--porcelain", "-z"])?;
    let mut entries = Vec::new();
    let normalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // `-z` terminates each field with NUL and separates worktree stanzas with
    // an empty NUL-terminated record, so paths containing newlines parse intact.
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut unusable = false;
    let mut flush =
        |path: &mut Option<PathBuf>, branch: &mut Option<String>, unusable: &mut bool| {
            let Some(p) = path.take() else {
                *branch = None;
                *unusable = false;
                return;
            };
            let b = branch.take();
            let skip = std::mem::take(unusable);
            if skip || !p.exists() {
                return;
            }
            let main = normalize(&p) == normalize(&root);
            let name = b.filter(|name| !name.is_empty()).unwrap_or_else(|| {
                p.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string_lossy().into_owned())
            });
            entries.push(WorktreeEntry {
                name,
                path: p,
                main,
            });
        };
    for field in output.split(|&b| b == 0) {
        if field.is_empty() {
            flush(&mut path, &mut branch, &mut unusable);
            continue;
        }
        if let Some(value) = field.strip_prefix(b"worktree ") {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                path = Some(PathBuf::from(std::ffi::OsStr::from_bytes(value)));
            }
            #[cfg(not(unix))]
            {
                path = Some(PathBuf::from(String::from_utf8_lossy(value).into_owned()));
            }
        } else if let Some(value) = field.strip_prefix(b"branch refs/heads/") {
            branch = Some(String::from_utf8_lossy(value).into_owned());
        } else if field == b"bare" || field.starts_with(b"prunable") {
            unusable = true;
        }
    }
    flush(&mut path, &mut branch, &mut unusable);
    Ok(entries)
}

/// Summary of how the current feature branch diverges from its base branch
/// (e.g. `main` or `master`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchDiffSummary {
    pub base: String,
    pub head: String,
    pub commits: usize,
    pub files: usize,
}

/// Detect the repository's base branch (`origin/HEAD` → `main` → `master`),
/// returning `None` when already checked out on that base branch or detached.
pub fn branch_diff_summary(dir: &Path) -> Option<BranchDiffSummary> {
    let root = repo_root(dir).ok()?;
    let head = git(
        &root,
        &["--no-optional-locks", "symbolic-ref", "--short", "HEAD"],
    )
    .ok()?;
    let head = head.trim().to_string();
    if head.is_empty() {
        return None;
    }

    let base = detect_base_branch(&root, &head)?;
    let rev_range = format!("{base}..HEAD");
    let commits = git(
        &root,
        &["--no-optional-locks", "rev-list", "--count", &rev_range],
    )
    .ok()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(0);
    let diff_range = format!("{base}...HEAD");
    let files = git(
        &root,
        &["--no-optional-locks", "diff", "--name-only", &diff_range],
    )
    .ok()
    .map(|out| out.lines().filter(|line| !line.trim().is_empty()).count())
    .unwrap_or(0);

    if commits == 0 && files == 0 {
        return None;
    }
    Some(BranchDiffSummary {
        base,
        head,
        commits,
        files,
    })
}

fn detect_base_branch(root: &Path, current_branch: &str) -> Option<String> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Ok(origin_head) = git(
        root,
        &[
            "--no-optional-locks",
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let short = origin_head
            .strip_prefix("origin/")
            .unwrap_or(&origin_head)
            .to_string();
        if !short.is_empty() {
            candidates.push((short.clone(), short));
            candidates.push((origin_head.clone(), origin_head));
        }
    }
    for name in ["main", "master"] {
        candidates.push((name.to_string(), name.to_string()));
        candidates.push((format!("origin/{name}"), name.to_string()));
    }

    for (git_ref, short_name) in candidates {
        if short_name == current_branch {
            return None;
        }
        if git(
            root,
            &[
                "--no-optional-locks",
                "rev-parse",
                "--verify",
                "--quiet",
                &git_ref,
            ],
        )
        .is_ok()
        {
            return Some(git_ref);
        }
    }
    None
}

/// Cumulative diff of the current branch relative to `base` (`base...HEAD`),
/// cut at [`MAX_PATCH_BYTES`].
pub fn branch_diff(dir: &Path, base: &str) -> Result<Patch, String> {
    let base = base.trim();
    if base.is_empty() || base.starts_with('-') {
        return Err("无效的基线分支名".to_string());
    }
    let root = repo_root(dir)?;
    let range = format!("{base}...HEAD");
    let args: Vec<std::ffi::OsString> = [
        "--no-optional-locks",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        &range,
    ]
    .into_iter()
    .map(std::ffi::OsString::from)
    .collect();
    read_patch(&root, &args, false)
}

/// Remove a linked git worktree at `dir` (deleting its working directory).
///
/// Refuses to remove the main checkout. When `force` is false, git refuses if
/// the worktree has uncommitted or untracked files.
pub fn remove_worktree(dir: &Path, force: bool) -> Result<(), String> {
    let wt_root = repo_root(dir)?;
    let common_raw = git(
        &wt_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .or_else(|_| git(&wt_root, &["rev-parse", "--git-common-dir"]))?;
    let common_path = {
        let p = PathBuf::from(&common_raw);
        if p.is_absolute() { p } else { wt_root.join(p) }
    };
    let main_root = common_path
        .parent()
        .ok_or_else(|| "无法确定主仓库目录".to_string())?
        .to_path_buf();
    let normalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if normalize(&wt_root) == normalize(&main_root) {
        return Err("不能删除主仓库目录，仅支持删除 linked worktree".to_string());
    }

    let mut args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("worktree"),
        std::ffi::OsStr::new("remove"),
    ];
    if force {
        args.push(std::ffi::OsStr::new("--force"));
    }
    args.push(std::ffi::OsStr::new("--"));
    args.push(wt_root.as_os_str());
    git_raw_os(&main_root, &args)?;
    Ok(())
}

/// The repository working tree that contains `dir`, which may be a
/// subdirectory of it.
fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let raw = git_raw(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|error| format!("不是 git 仓库：{error}"))?;
    let trimmed = raw
        .strip_suffix(b"\r\n")
        .or_else(|| raw.strip_suffix(b"\n"))
        .unwrap_or(&raw);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(trimmed)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(String::from_utf8_lossy(trimmed).into_owned()))
    }
}

/// Run git in `dir` and return trimmed stdout, or its stderr on failure.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git_raw(dir, args)?)
        .trim()
        .to_string())
}

/// Run git in `dir` and return stdout exactly as git wrote it, NUL separators
/// included.
fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let os_args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
    git_raw_os(dir, &os_args)
}

fn git_raw_os(dir: &Path, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("无法运行 git：{error}"))?;
    if !output.status.success() {
        let command = args
            .iter()
            .find_map(|arg| {
                let s = arg.to_str()?;
                (!s.starts_with('-')).then_some(s)
            })
            .unwrap_or("git");
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(git_failure(command, output.status, &stderr));
    }
    Ok(output.stdout)
}

/// The message for a git run that exited with `status`: git's stderr when it
/// said anything, otherwise just the command and its exit code.
fn git_failure(command: &str, status: ExitStatus, stderr: &str) -> String {
    let message = stderr.trim();
    if message.is_empty() {
        format!("git {command} 失败（退出码 {status}）")
    } else {
        message.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Parallel tests can read the clock in the same tick, so a counter keeps
    /// their fixture directories apart.
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    /// A repository with one commit in a fresh temporary directory, plus the
    /// directory that owns it so the caller can clean up both.
    fn fixture() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kumu-git-{}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        git(&repo, &["config", "tag.gpgsign", "false"]).unwrap();
        std::fs::write(repo.join("a"), "a").unwrap();
        git(&repo, &["add", "a"]).unwrap();
        git(&repo, &["commit", "-qm", "init"]).unwrap();
        (dir, repo)
    }

    /// A new branch gets its own sibling checkout without touching the
    /// repository it came from.
    #[test]
    fn creates_a_sibling_worktree_on_a_new_branch() {
        let (dir, repo) = fixture();
        let worktree = create_worktree(&repo, "feat/login").unwrap();
        assert_eq!(worktree.name, "feat/login");
        assert_eq!(
            worktree.path.canonicalize().unwrap(),
            dir.canonicalize().unwrap().join("repo-feat-login")
        );
        assert!(worktree.path.join(".git").exists());
        assert_eq!(
            git(&worktree.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "feat/login"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Existing worktrees are listed with their branch names, including a
    /// detached checkout that has no branch to name it after.
    #[test]
    fn lists_the_main_checkout_and_worktrees_created_outside_kumu() {
        let (dir, repo) = fixture();
        let linked = create_worktree(&repo, "feat/login").unwrap();
        let detached_path = dir.join("repo-detached");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                &detached_path.to_string_lossy(),
                "HEAD",
            ],
        )
        .unwrap();

        let entries = list_worktrees(&repo).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[0].main);
        assert_eq!(entries[0].path, repo.canonicalize().unwrap());
        let login = entries
            .iter()
            .find(|entry| entry.name == "feat/login")
            .expect("the branch worktree is listed");
        assert_eq!(login.path, linked.path.canonicalize().unwrap());
        assert!(!login.main);
        let detached = entries
            .iter()
            .find(|entry| entry.name == "repo-detached")
            .expect("a detached worktree is named after its directory");
        assert_eq!(detached.path, detached_path.canonicalize().unwrap());
        assert!(!detached.main);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Commit history is newest first, carries decorations and parent counts,
    /// and includes a root commit with no parents.
    #[test]
    fn lists_commits_newest_first_with_their_refs() {
        let (dir, repo) = fixture();
        std::fs::write(repo.join("a"), "changed").unwrap();
        git(&repo, &["add", "a"]).unwrap();
        git(&repo, &["commit", "-qm", "second", "-m", "Second body."]).unwrap();
        git(&repo, &["tag", "v1"]).unwrap();

        let history = history(&repo).unwrap();
        assert_eq!(
            history.branch,
            git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap()
        );
        assert_eq!(history.commits.len(), 2);
        let newest = &history.commits[0];
        assert_eq!(newest.subject, "second");
        assert_eq!(newest.body.trim(), "Second body.");
        assert_eq!(newest.parents, 1);
        assert!(newest.hash.starts_with(&newest.short));
        assert!(!newest.author.is_empty());
        assert!(newest.email.contains('@'));
        assert!(!newest.date.is_empty());
        assert!(newest.refs.iter().any(|name| name == "tag: v1"));
        let root = &history.commits[1];
        assert_eq!(root.subject, "init");
        assert_eq!(root.parents, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A merge commit's patch is its first parent's diff, and the commit
    /// message never ends up inside the patch.
    #[test]
    fn shows_a_commits_patch_but_not_its_message() {
        let (dir, repo) = fixture();
        let main = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        git(&repo, &["checkout", "-qb", "side"]).unwrap();
        std::fs::write(repo.join("b"), "b").unwrap();
        git(&repo, &["add", "b"]).unwrap();
        git(&repo, &["commit", "-qm", "side"]).unwrap();
        git(&repo, &["checkout", "-q", &main]).unwrap();
        std::fs::write(repo.join("c"), "c").unwrap();
        git(&repo, &["add", "c"]).unwrap();
        git(&repo, &["commit", "-qm", "main"]).unwrap();
        git(
            &repo,
            &["merge", "-q", "--no-ff", "side", "-m", "merged side"],
        )
        .unwrap();
        let head = git(&repo, &["rev-parse", "HEAD"]).unwrap();

        let diff = commit_diff(&repo, &head).unwrap();
        assert!(!diff.truncated);
        assert!(diff.patch.contains("+++ b/b"));
        assert!(!diff.patch.contains("merged side"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An oversized patch stops at the last complete file, and one too big for
    /// that keeps its start.
    #[test]
    fn cuts_an_oversized_patch_at_a_file_boundary() {
        let first = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n";
        let second = "diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-c\n+d\n";
        let patch = format!("{first}{second}");

        assert_eq!(
            trim_patch_at(patch.clone(), patch.len() - second.len() / 2),
            first
        );

        let trimmed = trim_patch_at(patch, 30);
        assert!(trimmed.len() <= 30);
        assert!(trimmed.starts_with("diff --git a/a"));
    }

    /// The read stops at the cap instead of buffering the whole patch, and git
    /// is killed rather than being waited on.
    #[test]
    fn stops_reading_a_patch_past_the_cap() {
        let (dir, repo) = fixture();
        let lines = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n".repeat(100_000);
        std::fs::write(repo.join("big"), lines).unwrap();
        git(&repo, &["add", "big"]).unwrap();
        git(&repo, &["commit", "-qm", "big"]).unwrap();

        let diff = commit_diff(&repo, "HEAD").unwrap();
        assert!(diff.truncated);
        assert!(diff.patch.len() <= MAX_PATCH_BYTES);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Staged, worktree-only and untracked files land in the section they
    /// belong to, and each patch is read from that side.
    #[test]
    fn lists_uncommitted_changes_and_reads_their_patches() {
        let (dir, repo) = fixture();
        // `a` is changed in the index and again in the working tree; `b` is a
        // staged new file; the untracked file's name has a space in it, which
        // the status format must not split on.
        std::fs::write(repo.join("a"), "staged\n").unwrap();
        git(&repo, &["add", "a"]).unwrap();
        std::fs::write(repo.join("a"), "worktree\n").unwrap();
        std::fs::write(repo.join("b"), "b\n").unwrap();
        git(&repo, &["add", "b"]).unwrap();
        std::fs::write(repo.join("new file"), "c\n").unwrap();

        let changes = changes(&repo).unwrap();
        assert_eq!(changes.len(), 4);
        let staged_a = changes.staged.iter().find(|c| c.path == "a").unwrap();
        assert_eq!(staged_a.status, 'M');
        assert!(staged_a.staged);
        let unstaged_a = changes.unstaged.iter().find(|c| c.path == "a").unwrap();
        assert_eq!(unstaged_a.status, 'M');
        let untracked = changes
            .unstaged
            .iter()
            .find(|c| c.path == "new file")
            .unwrap();
        assert_eq!(untracked.status, '?');

        // The index side and the working tree side are different patches.
        let staged = change_diff(&repo, staged_a).unwrap();
        assert!(staged.patch.contains("+staged\n"));
        assert!(!staged.patch.contains("+worktree\n"));
        let worktree = change_diff(&repo, unstaged_a).unwrap();
        assert!(worktree.patch.contains("+worktree\n"));
        let new_file = change_diff(&repo, untracked).unwrap();
        assert!(new_file.patch.contains("+c\n"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A rename is one row that remembers the path it came from, and both paths
    /// let git print the rename instead of a delete plus an add.
    #[test]
    fn lists_a_rename_with_its_original_path() {
        let (dir, repo) = fixture();
        git(&repo, &["mv", "a", "renamed"]).unwrap();

        let changes = changes(&repo).unwrap();
        assert_eq!(changes.staged.len(), 1);
        let rename = &changes.staged[0];
        assert_eq!(rename.path, "renamed");
        assert_eq!(rename.original.as_deref(), Some("a"));
        assert_eq!(rename.status, 'R');
        let patch = change_diff(&repo, rename).unwrap();
        assert!(patch.patch.contains("rename to renamed"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The poll's fingerprint holds still while the repository does, and moves
    /// for a new commit and for a new ref.
    #[test]
    fn fingerprint_follows_commits_and_refs() {
        let (dir, repo) = fixture();
        let before = fingerprint(&repo).unwrap();
        assert_eq!(before, fingerprint(&repo).unwrap());

        std::fs::write(repo.join("a"), "changed").unwrap();
        git(&repo, &["add", "a"]).unwrap();
        git(&repo, &["commit", "-qm", "second"]).unwrap();
        let committed = fingerprint(&repo).unwrap();
        assert_ne!(before, committed);

        git(&repo, &["tag", "v1"]).unwrap();
        assert_ne!(committed, fingerprint(&repo).unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn branch_diff_and_remove_worktree_handle_feature_branches_safely() {
        let (dir, repo) = fixture();
        // Rename initial branch to `main` so base-branch detection is deterministic.
        git(&repo, &["branch", "-M", "main"]).unwrap();
        assert_eq!(branch_diff_summary(&repo), None);

        // Main checkout must never be removable via remove_worktree.
        assert!(remove_worktree(&repo, false).is_err());

        let wt = create_worktree(&repo, "feat/branch-review").unwrap();
        std::fs::write(wt.path.join("a"), "hello\nfeature\n").unwrap();
        git(&wt.path, &["add", "a"]).unwrap();
        git(&wt.path, &["commit", "-qm", "feature change"]).unwrap();

        let summary = branch_diff_summary(&wt.path).expect("branch diff summary");
        assert_eq!(summary.base, "main");
        assert_eq!(summary.head, "feat/branch-review");
        assert_eq!(summary.commits, 1);
        assert_eq!(summary.files, 1);

        let patch = branch_diff(&wt.path, &summary.base).unwrap();
        assert!(patch.patch.contains("+feature"));

        // Dirty worktree refuses non-force removal, succeeds with force.
        std::fs::write(wt.path.join("dirty.txt"), "uncommitted").unwrap();
        assert!(remove_worktree(&wt.path, false).is_err());
        remove_worktree(&wt.path, true).unwrap();
        assert!(!wt.path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repo_branch_reports_branch_detached_and_non_git() {
        let (dir, repo) = fixture();
        git(&repo, &["branch", "-M", "main"]).unwrap();
        assert_eq!(repo_branch(&repo).as_deref(), Some("main"));

        let head = git(&repo, &["rev-parse", "--short", "HEAD"]).unwrap();
        git(&repo, &["checkout", "-q", "--detach", "HEAD"]).unwrap();
        assert_eq!(repo_branch(&repo).as_deref(), Some(head.as_str()));

        let plain = dir.join("not-a-repo");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(repo_branch(&plain), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
