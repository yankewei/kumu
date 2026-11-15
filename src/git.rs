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

/// What one commit changed, as a patch the diff view can parse.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CommitDiff {
    /// Unified patch in git's own format, possibly cut at [`MAX_PATCH_BYTES`].
    pub patch: String,
    /// The patch was cut, so the view has to say that it is incomplete.
    pub truncated: bool,
}

/// A commit that rewrites a generated file can produce a patch far bigger than
/// anyone reads, so the panel stops at this many bytes of patch text.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// Read the full history of `dir`'s checked-out branch. The panel virtualizes
/// the rows, so there is no limit here: "all commits" means every commit git
/// reaches from `HEAD`.
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
pub fn commit_diff(dir: &Path, hash: &str) -> Result<CommitDiff, String> {
    let root = repo_root(dir)?;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "--no-optional-locks",
            "show",
            "--format=",
            "--patch",
            "--no-color",
            "-m",
            "--first-parent",
            hash,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法运行 git：{error}"))?;

    // stderr is drained on its own thread: a chatty git must not fill the pipe
    // and block while the patch is read.
    let stderr = child.stderr.take().map(drain);
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_PATCH_BYTES as u64 + 1)
            .read_to_end(&mut output)
            .map_err(|error| format!("无法读取 git 输出：{error}"))?;
    }
    // Everything past the cap would be thrown away, so stop git instead of
    // waiting for output nobody reads.
    let truncated = output.len() > MAX_PATCH_BYTES;
    if truncated {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| format!("无法等待 git：{error}"))?;
    let stderr = stderr
        .map(|stderr| stderr.join().unwrap_or_default())
        .unwrap_or_default();
    if !truncated && !status.success() {
        return Err(git_failure("show", status, &stderr));
    }
    let patch = String::from_utf8_lossy(&output).into_owned();
    Ok(CommitDiff {
        patch: if truncated { trim_patch(patch) } else { patch },
        truncated,
    })
}

/// Read a child's stderr to the end on its own thread.
fn drain(mut pipe: ChildStderr) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut message = String::new();
        let _ = pipe.read_to_string(&mut message);
        message
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
    if branch.is_empty() {
        return Err("分支名不能为空".to_string());
    }
    let root = repo_root(dir)?;
    git(&root, &["check-ref-format", "--branch", branch])
        .map_err(|_| format!("「{branch}」不是有效的分支名"))?;

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
        git(&root, &["worktree", "add", &path_arg(&path), branch])?;
    } else {
        let path = path_arg(&path);
        git(&root, &["worktree", "add", "-b", branch, &path])?;
    }

    Ok(Worktree {
        name: branch.to_string(),
        path,
    })
}

/// Every worktree attached to the repository that contains `dir`, including
/// the main checkout. Missing directories (git still lists a worktree until it
/// is pruned) are left out: Kumu cannot open them.
pub fn list_worktrees(dir: &Path) -> Result<Vec<WorktreeEntry>, String> {
    let root = repo_root(dir)?;
    let output = git(&root, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let normalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // Porcelain output is one block per worktree, separated by a blank line.
    // Git lists the main worktree first, but the path comparison below is
    // what decides which entry it is.
    for block in output.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        let mut unusable = false;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
                branch = Some(value.to_string());
            }
            // A bare repository has no checkout, and a prunable entry has no
            // directory left to open.
            unusable |= line == "bare" || line.starts_with("prunable");
        }
        let Some(path) = path else {
            continue;
        };
        if unusable || !path.exists() {
            continue;
        }
        let main = normalize(&path) == normalize(&root);
        let name = branch.filter(|name| !name.is_empty()).unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        });
        entries.push(WorktreeEntry { name, path, main });
    }
    Ok(entries)
}

/// The repository working tree that contains `dir`, which may be a
/// subdirectory of it.
fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let output = git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|error| format!("不是 git 仓库：{error}"))?;
    Ok(PathBuf::from(output))
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("无法运行 git：{error}"))?;
    if !output.status.success() {
        let command = args
            .iter()
            .find(|arg| !arg.starts_with('-'))
            .copied()
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
    use super::*;

    /// A repository with one commit in a fresh temporary directory, plus the
    /// directory that owns it so the caller can clean up both.
    fn fixture() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kumu-git-{}-{}",
            std::process::id(),
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
                &path_arg(&detached_path),
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
}
