//! What a project's files are called, for referencing one in a prompt as
//! `@path`.
//!
//! A reference is only text — the agent runs in the project directory and
//! reads the file itself — so all kumu needs is the list of names to complete
//! from. Git's own view of the tree is used when there is one, because it
//! already knows what the project ignores; a plain walk that skips the usual
//! build and dependency folders stands in otherwise.

use std::path::Path;
use std::process::Command;

/// How many files the index holds, however they were listed, so pointing a
/// project at a home directory or a huge monorepo cannot stall the index or
/// the search that runs over it on every keystroke.
const LIST_LIMIT: usize = 20_000;

/// Folders a walk never enters: they are large, generated, and not what anyone
/// means to reference.
const SKIPPED_DIRS: [&str; 4] = ["node_modules", "target", "dist", "build"];

/// Every file under `root`, and every folder that holds one, relative to `root`
/// with `/` separators. Folders end in `/`, so a folder can be referenced too
/// and reads as one in the menu.
pub fn list(root: &Path) -> FileIndex {
    FileIndex::new(with_folders(git_files(root).unwrap_or_else(|| walk(root))))
}

/// A project's entries, each with its lowercase form made once when the list
/// is built rather than on every keystroke that searches it.
pub struct FileIndex {
    entries: Vec<String>,
    lower: Vec<String>,
}

impl FileIndex {
    fn new(entries: Vec<String>) -> Self {
        let lower = entries.iter().map(|entry| entry.to_lowercase()).collect();
        Self { entries, lower }
    }
}

/// The tracked and untracked-but-not-ignored files, as git lists them; `None`
/// outside a repository or without git.
fn git_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.split('\0')
            .filter(|path| !path.is_empty())
            .take(LIST_LIMIT)
            .map(str::to_string)
            .collect(),
    )
}

fn walk(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !SKIPPED_DIRS.contains(&name.as_ref()) {
                    pending.push(path);
                }
            } else if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_string_lossy().replace('\\', "/"));
                if files.len() >= LIST_LIMIT {
                    return files;
                }
            }
        }
    }
    files
}

/// The files plus each folder above them, sorted.
fn with_folders(files: Vec<String>) -> Vec<String> {
    let mut entries = std::collections::BTreeSet::new();
    for file in files {
        let mut end = 0;
        while let Some(slash) = file[end..].find('/') {
            end += slash + 1;
            entries.insert(file[..end].to_string());
        }
        entries.insert(file);
    }
    entries.into_iter().collect()
}

/// The entries a partly typed reference could mean, best first, at most
/// `limit` of them.
///
/// Matching is case-insensitive. A name that starts with the query beats one
/// that merely contains it, which beats a path that contains it, which beats a
/// path the query spells out in pieces (`uhc` finding
/// `src/ui/home/composer.rs`, see [`spelled_out`]); within a rank the shorter
/// path wins, since it is the likelier one to be meant. With nothing typed yet,
/// the top of the tree comes first.
pub fn rank<'a>(index: &'a FileIndex, query: &str, limit: usize) -> Vec<&'a str> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &str)> = index
        .entries
        .iter()
        .zip(&index.lower)
        .filter_map(|(entry, lower)| {
            let score = if query.is_empty() {
                entry
                    .trim_end_matches('/')
                    .matches('/')
                    .count()
                    .min(u8::MAX as usize) as u8
            } else {
                match_score(lower, &query)?
            };
            Some((score, entry.len(), entry.as_str()))
        })
        .collect();
    // Only the best `limit` are shown, so only they are sorted.
    if scored.len() > limit {
        scored.select_nth_unstable(limit);
        scored.truncate(limit);
    }
    scored.sort_unstable();
    scored.into_iter().map(|(_, _, entry)| entry).collect()
}

fn match_score(path: &str, query: &str) -> Option<u8> {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.starts_with(query) {
        Some(0)
    } else if name.contains(query) {
        Some(1)
    } else if path.contains(query) {
        Some(2)
    } else {
        spelled_out(path, query).then_some(3)
    }
}

/// Whether `query` is spelled out by `path` in pieces, each piece starting a
/// word of it — after a `/`, `_`, `-`, `.` or space — and running on from
/// there: `uhc` for `src/ui/home/composer.rs`, `sesli` for
/// `src/ui/session_list.rs`.
///
/// Characters merely in order would be looser still, and loose enough that an
/// ordinary word after an `@` (a person's name) matches some path in any real
/// project, turning the Enter meant to send into a completion.
fn spelled_out(path: &str, query: &str) -> bool {
    let path: Vec<char> = path.chars().collect();
    let starts_word = |i: usize| i == 0 || matches!(path[i - 1], '/' | '_' | '-' | '.' | ' ');
    // `ends[i]`: the query so far can end with its last character at `i`.
    let mut ends: Option<Vec<bool>> = None;
    for wanted in query.chars() {
        let mut next = vec![false; path.len()];
        let mut any_before = false;
        for i in 0..path.len() {
            if path[i] == wanted {
                next[i] = match &ends {
                    None => starts_word(i),
                    Some(ends) => (i > 0 && ends[i - 1]) || (any_before && starts_word(i)),
                };
            }
            if let Some(ends) = &ends {
                any_before |= ends[i];
            }
        }
        if !next.contains(&true) {
            return false;
        }
        ends = Some(next);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{FileIndex, rank, with_folders};

    fn entries() -> Vec<String> {
        with_folders(
            [
                "README.md",
                "src/main.rs",
                "src/app/session.rs",
                "src/ui/home/composer.rs",
                "src/ui/session_list.rs",
            ]
            .map(String::from)
            .to_vec(),
        )
    }

    #[test]
    fn folders_are_listed_with_their_files() {
        let entries = entries();
        for folder in ["src/", "src/app/", "src/ui/", "src/ui/home/"] {
            assert!(entries.iter().any(|e| e == folder), "{folder} is missing");
        }
    }

    /// A name that starts with the query outranks one that only contains it,
    /// and a shorter path wins a tie.
    #[test]
    fn names_that_start_with_the_query_come_first() {
        let entries = FileIndex::new(entries());
        assert_eq!(
            rank(&entries, "sess", 10),
            ["src/app/session.rs", "src/ui/session_list.rs"]
        );
        assert_eq!(rank(&entries, "COMP", 10), ["src/ui/home/composer.rs"]);
    }

    /// A path spelled out from the starts of its words still matches, but
    /// characters that are merely in order do not.
    #[test]
    fn word_starts_spell_out_a_path() {
        let entries = FileIndex::new(entries());
        assert_eq!(rank(&entries, "uhc", 10), ["src/ui/home/composer.rs"]);
        assert_eq!(rank(&entries, "sesli", 10), ["src/ui/session_list.rs"]);
        assert!(rank(&entries, "zzz", 10).is_empty());
        // In order in `src/ui/home/composer.rs`, but starting no words.
        assert!(rank(&entries, "rmc", 10).is_empty());
    }

    /// Nothing typed yet shows the top of the tree first, up to the limit.
    #[test]
    fn an_empty_query_starts_at_the_top() {
        let entries = FileIndex::new(entries());
        assert_eq!(rank(&entries, "", 2), ["src/", "README.md"]);
    }
}
