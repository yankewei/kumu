//! Git worktrees and commit history for projects.
//!
//! Kumu splits one repository into several projects without copying it: each
//! worktree is a sibling directory of the repository and a project row of its
//! own. A worktree created elsewhere can be attached to the same project list;
//! removing its row from Kumu leaves the directory alone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

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
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(if message.is_empty() {
            let command = args
                .iter()
                .find(|arg| !arg.starts_with('-'))
                .copied()
                .unwrap_or("git");
            format!("git {command} 失败（退出码 {}）", output.status)
        } else {
            message.to_string()
        });
    }
    Ok(output.stdout)
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
