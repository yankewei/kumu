//! Pure-Rust review domain & unified-diff line index for the Git panel.
//!
//! Zero GPUI imports: tracks review drafts, sent receipts, comment prompt
//! formatting, and exact mapping between `nsh_base::DiffView`'s joined 0-based
//! patch coordinates and real `(file_path, 1-based line)` locations.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use sha2::{Digest, Sha256};

use crate::git::{Change, Patch};
use crate::plugins::PluginHost;

/// Which side of a diff a review comment targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ReviewSide {
    Original,
    Modified,
}

impl From<nsh_base::DiffSide> for ReviewSide {
    fn from(side: nsh_base::DiffSide) -> Self {
        match side {
            nsh_base::DiffSide::Old => Self::Original,
            nsh_base::DiffSide::New => Self::Modified,
        }
    }
}

impl From<ReviewSide> for nsh_base::DiffSide {
    fn from(side: ReviewSide) -> Self {
        match side {
            ReviewSide::Original => Self::Old,
            ReviewSide::Modified => Self::New,
        }
    }
}

/// A 1-based line range within one file of a patch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReviewRange {
    pub(super) path: String,
    pub(super) side: ReviewSide,
    pub(super) end_side: ReviewSide,
    pub(super) start: usize,
    pub(super) end: usize,
}

impl ReviewRange {
    pub(super) fn new(path: impl Into<String>, side: ReviewSide, start: usize, end: usize) -> Self {
        Self {
            path: path.into(),
            side,
            end_side: side,
            start,
            end,
        }
    }

    #[cfg(test)]
    pub(super) fn with_end_side(mut self, end_side: ReviewSide) -> Self {
        self.end_side = end_side;
        self
    }

    pub(super) fn path(&self) -> &str {
        &self.path
    }

    pub(super) fn side(&self) -> ReviewSide {
        self.side
    }

    pub(super) fn end_side(&self) -> ReviewSide {
        self.end_side
    }

    pub(super) fn start(&self) -> usize {
        self.start
    }

    pub(super) fn end(&self) -> usize {
        self.end
    }
}

/// Exact mapping between `nsh_base::DiffView`'s joined 0-based line indices
/// (across all files in a unified patch) and `(file_path, 1-based line)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PatchIndex {
    pub(super) file_count: usize,
    /// `old_lines[joined_0_based] = (file_path, 1_based_old_line)`
    pub(super) old_lines: Vec<(String, usize)>,
    /// `new_lines[joined_0_based] = (file_path, 1_based_new_line)`
    pub(super) new_lines: Vec<(String, usize)>,
}

impl PatchIndex {
    /// Resolve an `nsh_base::DiffLineRange` (0-based joined lines on one side)
    /// into a real 1-based [`ReviewRange`].
    pub(super) fn resolve_range(&self, range: nsh_base::DiffLineRange) -> Option<ReviewRange> {
        let side = ReviewSide::from(range.side);
        let table = match side {
            ReviewSide::Original => &self.old_lines,
            ReviewSide::Modified => &self.new_lines,
        };
        let (path, start) = table.get(range.start as usize)?;
        let end = table
            .get(range.end as usize)
            .filter(|(p, _)| p == path)
            .map_or(*start, |(_, line)| *line);
        let (lo, hi) = if *start <= end {
            (*start, end)
        } else {
            (end, *start)
        };
        Some(ReviewRange::new(path.clone(), side, lo, hi))
    }

    /// Map a persisted [`ReviewRange`] back to a joined 0-based
    /// `(DiffSide, line)` anchor for `nsh_base::DiffAnnotation`.
    pub(super) fn anchor_for(&self, range: &ReviewRange) -> Option<(nsh_base::DiffSide, u32)> {
        let table = match range.side {
            ReviewSide::Original => &self.old_lines,
            ReviewSide::Modified => &self.new_lines,
        };
        let target_line = range.end.max(range.start);
        let idx = table
            .iter()
            .position(|(p, l)| p == &range.path && *l == target_line)
            .or_else(|| {
                table
                    .iter()
                    .position(|(p, l)| p == &range.path && *l == range.start)
            })?;
        Some((range.side.into(), idx as u32))
    }
}

fn parse_hunk_coords(header: &str) -> Option<(usize, usize, usize, usize, String)> {
    let rest = header.strip_prefix("@@ ")?;
    let (coords, suffix) = rest.split_once(" @@")?;
    let mut parts = coords.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;
    let parse_pair = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse_pair(old_part)?;
    let (new_start, new_count) = parse_pair(new_part)?;
    Some((
        old_start,
        old_count,
        new_start,
        new_count,
        suffix.to_string(),
    ))
}

fn strip_git_prefix(raw: &str, prefix: &str) -> Option<String> {
    let s = raw.split('\t').next().unwrap_or(raw).trim();
    if s == "/dev/null" {
        return None;
    }
    let unquoted = s.trim_matches('"');
    Some(
        unquoted
            .strip_prefix(prefix)
            .unwrap_or(unquoted)
            .to_string(),
    )
}

/// Normalize a raw unified diff (fixing any truncated final hunk header counts
/// so `nsh_base::DiffView` always parses it cleanly) and build a [`PatchIndex`]
/// whose joined 0-based line indices match `nsh_base`'s `diff/files.rs` 1-to-1.
pub(super) fn normalize_and_index_patch(raw: &str, truncated: bool) -> (String, PatchIndex) {
    let mut lines: Vec<&str> = raw.split('\n').collect();
    if raw.ends_with('\n') {
        lines.pop();
    }

    let mut out_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut index = PatchIndex::default();
    let mut current_old_path: Option<String> = None;
    let mut current_new_path: Option<String> = None;
    let mut in_git = false;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].strip_suffix('\r').unwrap_or(lines[i]);
        if let Some(names) = line.strip_prefix("diff --git ") {
            in_git = true;
            index.file_count += 1;
            current_old_path = None;
            current_new_path = None;
            if let Some((a, b)) = names.split_once(" b/") {
                current_old_path = Some(a.strip_prefix("a/").unwrap_or(a).trim_matches('"').into());
                current_new_path = Some(b.trim_matches('"').into());
            }
            out_lines.push(line.to_string());
            i += 1;
            continue;
        }
        if line.starts_with("--- ")
            && lines
                .get(i + 1)
                .is_some_and(|next| next.starts_with("+++ "))
        {
            if !in_git {
                index.file_count += 1;
            }
            let next = lines[i + 1].strip_suffix('\r').unwrap_or(lines[i + 1]);
            current_old_path = strip_git_prefix(&line[4..], if in_git { "a/" } else { "" });
            current_new_path = strip_git_prefix(&next[4..], if in_git { "b/" } else { "" });
            out_lines.push(line.to_string());
            out_lines.push(next.to_string());
            i += 2;
            continue;
        }
        if line.starts_with("@@ ")
            && let Some((old_start, old_count, new_start, new_count, suffix)) =
                parse_hunk_coords(line)
        {
            if index.file_count == 0 {
                index.file_count = 1;
            }
            let path = current_new_path
                .clone()
                .or_else(|| current_old_path.clone())
                .unwrap_or_default();
            let (mut old_left, mut new_left) = (old_count, new_count);
            let mut actual_old = 0usize;
            let mut actual_new = 0usize;
            let mut hunk_body: Vec<String> = Vec::new();
            let mut old_cursor = old_start.max(1);
            let mut new_cursor = new_start.max(1);

            i += 1;
            while i < lines.len() {
                let next = lines[i];
                if old_left == 0 && new_left == 0 && !next.starts_with('\\') {
                    break;
                }
                match next.as_bytes().first() {
                    Some(b'\\') => {
                        hunk_body.push(next.to_string());
                    }
                    Some(b'+') if new_left > 0 => {
                        index.new_lines.push((path.clone(), new_cursor));
                        new_cursor += 1;
                        actual_new += 1;
                        new_left -= 1;
                        hunk_body.push(next.to_string());
                    }
                    Some(b'-') if old_left > 0 => {
                        index.old_lines.push((path.clone(), old_cursor));
                        old_cursor += 1;
                        actual_old += 1;
                        old_left -= 1;
                        hunk_body.push(next.to_string());
                    }
                    Some(b' ') | None if old_left > 0 && new_left > 0 => {
                        index.old_lines.push((path.clone(), old_cursor));
                        index.new_lines.push((path.clone(), new_cursor));
                        old_cursor += 1;
                        new_cursor += 1;
                        actual_old += 1;
                        actual_new += 1;
                        old_left -= 1;
                        new_left -= 1;
                        hunk_body.push(next.to_string());
                    }
                    _ => break,
                }
                i += 1;
            }

            if truncated && (old_left > 0 || new_left > 0) {
                out_lines.push(format!(
                    "@@ -{old_start},{actual_old} +{new_start},{actual_new} @@{suffix}"
                ));
            } else {
                out_lines.push(line.to_string());
            }
            out_lines.extend(hunk_body);
            continue;
        }

        out_lines.push(line.to_string());
        i += 1;
    }

    let mut normalized = out_lines.join("\n");
    if !normalized.is_empty() {
        normalized.push('\n');
    }
    (normalized, index)
}

/// Where a [`PatchView`] reads its patch from.
#[derive(Clone)]
pub(super) enum PatchSource {
    /// One commit in the repository at `root`.
    Commit { root: PathBuf, hash: String },
    /// One uncommitted change in the repository at `root`.
    Change { root: PathBuf, change: Change },
    /// Cumulative diff of the current branch relative to `base` (`base...HEAD`).
    BranchDiff {
        root: PathBuf,
        base: String,
        head: String,
    },
}

impl PatchSource {
    /// Where drafts and sent records are filed: one entry per patch the user
    /// can open.
    pub(super) fn key(&self, project: i64) -> PatchKey {
        match self {
            Self::Commit { hash, .. } => PatchKey::commit(project, hash),
            Self::Change { change, .. } => PatchKey::change(project, change),
            Self::BranchDiff { base, head, .. } => PatchKey::branch_diff(project, base, head),
        }
    }

    /// Read the patch from git without parsing it.
    pub(super) fn read(&self) -> Result<Patch, String> {
        match self {
            Self::Commit { root, hash } => crate::git::commit_diff(root, hash),
            Self::Change { root, change } => crate::git::change_diff(root, change),
            Self::BranchDiff { root, base, .. } => crate::git::branch_diff(root, base),
        }
    }
}

/// One patch's identity inside a project. Drafts and sent records survive a
/// closed sheet under this key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct PatchKey {
    project: i64,
    target: String,
}

impl PatchKey {
    pub(super) fn commit(project: i64, hash: &str) -> Self {
        Self {
            project,
            target: format!("commit:{hash}"),
        }
    }

    pub(super) fn change(project: i64, change: &Change) -> Self {
        Self {
            project,
            target: format!(
                "change:{}:{}",
                if change.staged { "index" } else { "worktree" },
                change.path
            ),
        }
    }

    pub(super) fn branch_diff(project: i64, base: &str, head: &str) -> Self {
        Self {
            project,
            target: format!("branch:{base}...{head}"),
        }
    }
}

/// One comment as it was when its sheet closed, so reopening the patch brings
/// the editors back with what the user typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftComment {
    pub(super) range: ReviewRange,
    pub(super) text: String,
}

/// The last review a patch sent, shown on its row as a receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SentReview {
    pub(super) count: usize,
    pub(super) target: String,
}

/// The panel's review memory: unsent drafts and the reviews already sent.
#[derive(Default)]
pub(super) struct ReviewStore {
    pub(super) drafts: HashMap<PatchKey, Vec<DraftComment>>,
    pub(super) sent: HashMap<PatchKey, SentReview>,
}

/// Which marker badge a Git panel row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewMark {
    Draft,
    Sent,
}

/// A pure-Rust snapshot of [`ReviewStore`] for row rendering.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewMarks {
    drafts: HashSet<PatchKey>,
    sent: HashMap<PatchKey, SentReview>,
}

impl ReviewMarks {
    pub(super) fn of(store: &ReviewStore) -> Self {
        Self {
            drafts: store.drafts.keys().cloned().collect(),
            sent: store.sent.clone(),
        }
    }

    pub(super) fn mark(&self, key: &PatchKey) -> Option<ReviewMark> {
        if self.drafts.contains(key) {
            return Some(ReviewMark::Draft);
        }
        self.sent.contains_key(key).then_some(ReviewMark::Sent)
    }
}

/// Everything a patch sheet needs before it reads the patch.
pub(super) struct PatchRequest {
    pub(super) project: i64,
    pub(super) title: String,
    pub(super) meta: String,
    pub(super) source: PatchSource,
    /// The commit message, shown above the patch; empty for a change.
    pub(super) message: String,
    /// Where the patch's line numbers point, named when they are sent to the
    /// agent: a commit's numbers are not the working tree's.
    pub(super) context: String,
}

/// One review comment, edited beneath the line it targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewComment {
    pub(super) index: u64,
    pub(super) range: ReviewRange,
    pub(super) text: String,
}

impl ReviewComment {
    pub(super) fn annotation_id(&self) -> String {
        format!("git-diff-comment-{}", self.index)
    }
}

/// Why the sheet could not submit, or why it left the text on a shell's
/// command line instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SendNotice {
    /// The text is on the named pane's command line, where Enter would run it.
    Pasted(String),
    Hint(String),
}

/// What a sheet opens with: the patch it shows, where the patch comes from,
/// and the panel's review memory it reads and writes.
pub(super) struct PatchSheet {
    pub(super) project: i64,
    pub(super) title: String,
    pub(super) meta: String,
    pub(super) key: PatchKey,
    pub(super) source: PatchSource,
    pub(super) message: String,
    pub(super) context: String,
    pub(super) host: PluginHost,
    pub(super) reviews: Rc<RefCell<ReviewStore>>,
    pub(super) notify_panel: Rc<dyn Fn(&mut gpui::App)>,
}

/// A patch's identity for the watcher: the text and whether it was cut.
pub(super) fn patch_digest(patch: &Patch) -> String {
    let mut hasher = Sha256::new();
    hasher.update(patch.patch.as_bytes());
    hasher.update([patch.truncated as u8]);
    format!("{:x}", hasher.finalize())
}

/// Where a comment points, in the form the agent reads in the patch. A range
/// can start on one side and end on the other in the unified view.
pub(super) fn comment_location(range: &ReviewRange) -> String {
    let path = range.path();
    if range.side() != range.end_side() {
        return format!(
            "{path}:{}（改动前）-{}（改动后）",
            range.start(),
            range.end()
        );
    }
    let lines = if range.start() == range.end() {
        range.start().to_string()
    } else {
        format!("{}-{}", range.start(), range.end())
    };
    if range.side() == ReviewSide::Original {
        format!("{path}:{lines}（改动前）")
    } else {
        format!("{path}:{lines}")
    }
}

/// Every comment as one instruction, each under the line it targets.
pub(super) fn compose_comments(context: &str, comments: &[(ReviewRange, String)]) -> String {
    let mut message = if context.is_empty() {
        String::from("请根据以下代码审查意见修改：")
    } else {
        format!("请根据以下代码审查意见修改（{context}）：")
    };
    for (range, comment) in comments {
        message.push_str("\n\n");
        message.push_str(&comment_location(range));
        message.push('\n');
        message.push_str(comment);
    }
    message
}
