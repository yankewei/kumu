use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::Theme;
use gpui_kit::component::diff::{DiffLineRange, DiffSide};
use gpui_kit::component::input::TextareaState;
use gpui_kit::prelude::*;
use gpui_kit::*;
use sha2::{Digest, Sha256};

use crate::git::{Change, Patch};
use crate::plugins::PluginHost;

/// Where a [`PatchView`] reads its patch from.
#[derive(Clone)]
pub(super) enum PatchSource {
    /// One commit in the repository at `root`.
    Commit { root: PathBuf, hash: String },
    /// One uncommitted change in the repository at `root`.
    Change { root: PathBuf, change: Change },
}

impl PatchSource {
    /// Where drafts and sent records are filed: one entry per patch the user
    /// can open.
    pub(super) fn key(&self, project: i64) -> PatchKey {
        match self {
            Self::Commit { hash, .. } => PatchKey::commit(project, hash),
            Self::Change { change, .. } => PatchKey::change(project, change),
        }
    }

    /// Read the patch from git without parsing it.
    pub(super) fn read(&self) -> Result<Patch, String> {
        match self {
            Self::Commit { root, hash } => crate::git::commit_diff(root, hash),
            Self::Change { root, change } => crate::git::change_diff(root, change),
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
}

/// One comment as it was when its sheet closed, so reopening the patch brings
/// the editors back with what the user typed.
#[derive(Clone)]
pub(super) struct DraftComment {
    pub(super) range: DiffLineRange,
    pub(super) text: String,
}

/// The last review a patch sent, shown on its row as a receipt.
#[derive(Clone)]
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

/// A snapshot of [`ReviewStore`] the panel's row closures can carry.
#[derive(Default, Clone)]
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

    /// The small mark a patch row wears: a draft marker while a comment waits,
    /// or a receipt once a review reached a pane. A draft takes precedence,
    /// because it is the one thing still asking for the user.
    pub(super) fn mark(&self, key: &PatchKey, theme: &Theme) -> Option<AnyElement> {
        if self.drafts.contains(key) {
            return Some(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.warning)
                    .child("草稿")
                    .into_any_element(),
            );
        }
        self.sent.get(key).map(|_| {
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.success)
                .child("已审查")
                .into_any_element()
        })
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
pub(super) struct ReviewComment {
    /// Distinguishes this comment's elements from an earlier one's.
    pub(super) index: u64,
    pub(super) range: DiffLineRange,
    /// The latest text, kept even before a textarea exists, so a restored
    /// comment is never mistaken for an empty one.
    pub(super) text: String,
    /// Created on the first paint, which is the first moment a window exists.
    pub(super) input: Option<Entity<TextareaState>>,
}

impl ReviewComment {
    pub(super) fn annotation_id(&self) -> ElementId {
        ElementId::named_usize("git-diff-comment", self.index as usize)
    }
}

/// Why the sheet could not submit, or why it left the text on a shell's
/// command line instead.
pub(super) enum SendNotice {
    /// The text is on the named pane's command line, where Enter would run it.
    Pasted(String),
    Hint(String),
}

/// What a sheet opens with: the patch it shows, where the patch comes from,
/// and the panel's review memory it reads and writes.
pub(super) struct PatchSheet {
    pub(super) project: i64,
    pub(super) key: PatchKey,
    pub(super) source: PatchSource,
    /// The commit message, shown above the patch; empty for a change.
    pub(super) message: String,
    /// Where the patch's line numbers point, named when they are sent.
    pub(super) context: String,
    pub(super) host: PluginHost,
    pub(super) reviews: Rc<RefCell<ReviewStore>>,
    pub(super) notify_panel: Rc<dyn Fn(&mut App)>,
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
pub(super) fn comment_location(range: &DiffLineRange) -> String {
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
    if range.side() == DiffSide::Original {
        format!("{path}:{lines}（改动前）")
    } else {
        format!("{path}:{lines}")
    }
}

/// Every comment as one instruction, each under the line it targets.
pub(super) fn compose_comments(context: &str, comments: &[(DiffLineRange, String)]) -> String {
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
