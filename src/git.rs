//! Git worktrees for projects.
//!
//! Kumu splits one repository into several projects without copying it: each
//! worktree is a sibling directory of the repository and a project row of its
//! own. Only creation lives here; removing a worktree row from Kumu leaves its
//! directory alone.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A worktree created beside the repository it belongs to.
pub struct Worktree {
    /// Branch checked out in the new worktree; this becomes its project name.
    pub name: String,
    pub path: PathBuf,
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
            format!("git {} 失败（退出码 {}）", args[0], output.status)
        } else {
            message.to_string()
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new branch gets its own sibling checkout without touching the
    /// repository it came from.
    #[test]
    fn creates_a_sibling_worktree_on_a_new_branch() {
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
}
