//! Pure-Rust review domain & unified-diff line index for the Git panel.
//!
//! Zero GPUI imports: tracks review drafts, sent receipts, comment prompt
//! formatting, and exact mapping between `nsh_base::DiffView`'s joined 0-based
//! patch coordinates and real `(file_path, 1-based line)` locations.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git::{Change, Patch, WorkingTreeDiffScope};
use crate::plugins::PluginHost;

/// Which side of a diff a review comment targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Summary of one file inside a parsed unified patch, used for the left file
/// navigation sidebar and total `+added / −deleted` stats.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PatchFileSummary {
    pub(super) path: String,
    pub(super) old_path: Option<String>,
    /// `'M'`, `'A'`, `'D'`, `'R'`, or `'C'`.
    pub(super) status: char,
    pub(super) added: usize,
    pub(super) deleted: usize,
    pub(super) binary: bool,
}

/// Exact mapping between `nsh_base::DiffView`'s joined 0-based line indices
/// (across all files in a unified patch) and `(file_path, 1-based line)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PatchIndex {
    pub(super) file_count: usize,
    pub(super) files: Vec<PatchFileSummary>,
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
            let path = current_new_path
                .clone()
                .or_else(|| current_old_path.clone())
                .unwrap_or_default();
            index.files.push(PatchFileSummary {
                path,
                old_path: None,
                status: 'M',
                added: 0,
                deleted: 0,
                binary: false,
            });
            out_lines.push(line.to_string());
            i += 1;
            continue;
        }
        if let Some(file) = index.files.last_mut() {
            if line.starts_with("new file mode ") {
                file.status = 'A';
            } else if line.starts_with("deleted file mode ") {
                file.status = 'D';
            } else if let Some(from) = line.strip_prefix("rename from ") {
                file.status = 'R';
                file.old_path = Some(from.trim().to_string());
            } else if let Some(to) = line.strip_prefix("rename to ") {
                file.status = 'R';
                file.path = to.trim().to_string();
            } else if let Some(from) = line.strip_prefix("copy from ") {
                file.status = 'C';
                file.old_path = Some(from.trim().to_string());
            } else if let Some(to) = line.strip_prefix("copy to ") {
                file.status = 'C';
                file.path = to.trim().to_string();
            } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                file.binary = true;
            }
        }
        if line.starts_with("--- ")
            && lines
                .get(i + 1)
                .is_some_and(|next| next.starts_with("+++ "))
        {
            let next = lines[i + 1].strip_suffix('\r').unwrap_or(lines[i + 1]);
            current_old_path = strip_git_prefix(&line[4..], if in_git { "a/" } else { "" });
            current_new_path = strip_git_prefix(&next[4..], if in_git { "b/" } else { "" });
            if !in_git {
                index.file_count += 1;
                let path = current_new_path
                    .clone()
                    .or_else(|| current_old_path.clone())
                    .unwrap_or_default();
                let status = match (&current_old_path, &current_new_path) {
                    (None, Some(_)) => 'A',
                    (Some(_), None) => 'D',
                    _ => 'M',
                };
                index.files.push(PatchFileSummary {
                    path,
                    old_path: None,
                    status,
                    added: 0,
                    deleted: 0,
                    binary: false,
                });
            } else if let Some(file) = index.files.last_mut() {
                if let Some(p) = current_new_path
                    .clone()
                    .or_else(|| current_old_path.clone())
                {
                    file.path = p;
                }
                if current_old_path.is_none() && current_new_path.is_some() {
                    file.status = 'A';
                } else if current_old_path.is_some() && current_new_path.is_none() {
                    file.status = 'D';
                }
            }
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
                index.files.push(PatchFileSummary {
                    path: current_new_path
                        .clone()
                        .or_else(|| current_old_path.clone())
                        .unwrap_or_default(),
                    old_path: None,
                    status: 'M',
                    added: 0,
                    deleted: 0,
                    binary: false,
                });
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
                        if let Some(file) = index.files.last_mut() {
                            file.added += 1;
                        }
                        new_cursor += 1;
                        actual_new += 1;
                        new_left -= 1;
                        hunk_body.push(next.to_string());
                    }
                    Some(b'-') if old_left > 0 => {
                        index.old_lines.push((path.clone(), old_cursor));
                        if let Some(file) = index.files.last_mut() {
                            file.deleted += 1;
                        }
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
    /// Combined uncommitted changes in the working tree / index at `root`.
    WorkingTree {
        root: PathBuf,
        scope: WorkingTreeDiffScope,
    },
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
            Self::WorkingTree { scope, .. } => PatchKey::working_tree(project, *scope),
            Self::BranchDiff { base, head, .. } => PatchKey::branch_diff(project, base, head),
        }
    }

    /// Read the patch from git without parsing it.
    pub(super) fn read(&self) -> Result<Patch, String> {
        match self {
            Self::Commit { root, hash } => crate::git::commit_diff(root, hash),
            Self::WorkingTree { root, scope } => crate::git::working_tree_diff(root, *scope),
            Self::BranchDiff { root, base, .. } => crate::git::branch_diff(root, base),
        }
    }

    /// Returns the active working-tree scope if this source is a combined working-tree diff.
    pub(super) fn working_tree_scope(&self) -> Option<WorkingTreeDiffScope> {
        match self {
            Self::WorkingTree { scope, .. } => Some(*scope),
            _ => None,
        }
    }
}

/// One patch's identity inside a project. Drafts and sent records survive a
/// closed sheet under this key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    pub(super) fn working_tree(project: i64, scope: WorkingTreeDiffScope) -> Self {
        let name = match scope {
            WorkingTreeDiffScope::All => "all",
            WorkingTreeDiffScope::Staged => "staged",
            WorkingTreeDiffScope::Unstaged => "unstaged",
        };
        Self {
            project,
            target: format!("working-tree:{name}"),
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DraftComment {
    pub(super) range: ReviewRange,
    pub(super) text: String,
}

/// The last review a patch sent, shown on its row as a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// JSON shape of [`ReviewStore`]. Map keys are not strings, so reviews are
/// stored as ordered pairs. Entries are sorted to keep the document stable
/// across saves.
#[derive(Default, Serialize, Deserialize)]
struct PersistedReviews {
    #[serde(default)]
    drafts: Vec<(PatchKey, Vec<DraftComment>)>,
    #[serde(default)]
    sent: Vec<(PatchKey, SentReview)>,
}

/// Setting key holding every repository's review drafts and receipts.
const REVIEW_STORE_SETTING: &str = "git_reviews";
/// A safety valve for a pathological draft document; review notes are small.
const MAX_REVIEW_STORE_BYTES: usize = 4 * 1024 * 1024;

impl ReviewStore {
    /// Load the persisted drafts and receipts. A missing or unreadable document
    /// simply means there is nothing to restore.
    pub(super) fn load() -> Self {
        let Some(json) = crate::db::get_setting(REVIEW_STORE_SETTING) else {
            return Self::default();
        };
        let Ok(persisted) = serde_json::from_str::<PersistedReviews>(&json) else {
            eprintln!("[git] 忽略无法解析的 review 记录");
            return Self::default();
        };
        Self {
            drafts: persisted.drafts.into_iter().collect(),
            sent: persisted.sent.into_iter().collect(),
        }
    }

    /// Write drafts and receipts so closing or switching the Git panel no
    /// longer loses comments the user has not sent yet.
    pub(super) fn persist(&self) {
        let mut drafts: Vec<_> = self
            .drafts
            .iter()
            .map(|(key, comments)| (key.clone(), comments.clone()))
            .collect();
        drafts.sort_by(|(a, _), (b, _)| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.target.cmp(&b.target))
        });
        let mut sent: Vec<_> = self
            .sent
            .iter()
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect();
        sent.sort_by(|(a, _), (b, _)| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.target.cmp(&b.target))
        });
        let persisted = PersistedReviews { drafts, sent };
        let Ok(json) = serde_json::to_string(&persisted) else {
            return;
        };
        if json.len() > MAX_REVIEW_STORE_BYTES {
            eprintln!("[git] review 记录超过上限，未保存");
            return;
        }
        if let Err(error) = crate::db::set_setting(REVIEW_STORE_SETTING, &json) {
            eprintln!("[git] 保存 review 记录失败：{error}");
        }
    }
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
    /// Optional file path to scroll to immediately once the multi-file patch loads.
    pub(super) initial_file: Option<String>,
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
    pub(super) initial_file: Option<String>,
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

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn review_drafts_and_receipts_round_trip_through_json() {
        let key = PatchKey::commit(7, "abc123");
        let persisted = PersistedReviews {
            drafts: vec![(
                key.clone(),
                vec![DraftComment {
                    range: ReviewRange::new("src/a.rs", ReviewSide::Modified, 3, 5),
                    text: "这里需要处理空值".into(),
                }],
            )],
            sent: vec![(
                key,
                SentReview {
                    count: 2,
                    target: "Codex".into(),
                },
            )],
        };
        let json = serde_json::to_string(&persisted).expect("serialize reviews");
        let restored: PersistedReviews = serde_json::from_str(&json).expect("restore reviews");
        assert_eq!(restored.drafts.len(), 1);
        assert_eq!(restored.drafts[0].1[0].text, "这里需要处理空值");
        assert_eq!(restored.drafts[0].1[0].range.path, "src/a.rs");
        assert_eq!(restored.sent[0].1.count, 2);
        assert_eq!(restored.sent[0].1.target, "Codex");
    }
}
