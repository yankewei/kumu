//! OSC 7501 Program Status record table, per-app aggregation, and text sanitization.
//!
//! `libghostty-vt` handles wire-level parsing, capability probe replies, base64
//! decoding, and C0 stripping, then delivers each valid report synchronously to
//! [`ProgramStatusTable::apply`]. Records are aggregated per resolved `app` so
//! nested tools (such as `cargo` or `terraform`) cannot mask or hijack an
//! agent's status.

use std::collections::{BTreeSet, HashMap};

use super::types::{ProgramBlockKind, ProgramState, TermProgramStatus};

/// Maximum active records tracked per terminal (spec: allow >= 64, keep <= 256).
pub(super) const MAX_PROGRAM_STATUS_RECORDS: usize = 128;

/// Maximum queued aggregated status updates waiting for the UI thread.
pub(in crate::term) const MAX_PROGRAM_STATUSES: usize = 128;

const MAX_APP_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 160;
const MAX_MESSAGE_CHARS: usize = 512;

/// Borrowed report decoded by `libghostty-vt` for a single OSC 7501 sequence.
#[derive(Clone, Copy, Debug)]
pub(super) struct ProgramStatusReport<'a> {
    pub state: ProgramState,
    pub kind: Option<ProgramBlockKind>,
    pub progress: Option<u8>,
    pub id: &'a str,
    pub app: &'a str,
    pub title: &'a str,
    pub message: &'a str,
}

#[derive(Clone, Debug)]
struct Record {
    state: ProgramState,
    kind: Option<ProgramBlockKind>,
    app: String,
    title: String,
    progress: Option<u8>,
    message: String,
    seq: u64,
}

/// Per-terminal table of active OSC 7501 records keyed by hierarchical `id`.
#[derive(Default)]
pub(super) struct ProgramStatusTable {
    records: HashMap<String, Record>,
    next_seq: u64,
}

impl ProgramStatusTable {
    pub(super) fn clear_all(&mut self) {
        self.records.clear();
    }

    /// Remove `working`, `blocked`, and `idle` records when a new primary shell
    /// prompt starts (`OSC 133;A` / `GHOSTTY_SEMANTIC_PROMPT_PROMPT_START`),
    /// preserving `done` and `error` records per Ghostty's specification.
    pub(super) fn clear_on_prompt_start(&mut self) -> Vec<TermProgramStatus> {
        let mut affected_apps = BTreeSet::new();
        let mut keys_to_remove = Vec::new();
        for (key, record) in &self.records {
            if matches!(
                record.state,
                ProgramState::Working | ProgramState::Blocked | ProgramState::Idle
            ) {
                affected_apps.insert(self.resolve_app(key));
                keys_to_remove.push(key.clone());
            }
        }
        if keys_to_remove.is_empty() {
            return Vec::new();
        }
        for key in keys_to_remove {
            self.records.remove(&key);
        }
        affected_apps
            .into_iter()
            .map(|app| self.aggregate_for_app(&app))
            .collect()
    }

    pub(super) fn apply(&mut self, report: ProgramStatusReport<'_>) -> Vec<TermProgramStatus> {
        if report.state == ProgramState::Clear {
            let mut affected_apps = BTreeSet::new();
            for key in self.records.keys() {
                if report.id.is_empty() || key == report.id || is_descendant(key, report.id) {
                    affected_apps.insert(self.resolve_app(key));
                }
            }
            if affected_apps.is_empty() {
                let explicit_app = sanitize_external_text(report.app, MAX_APP_CHARS);
                let fallback = if !explicit_app.is_empty() {
                    explicit_app
                } else {
                    self.resolve_app(report.id)
                };
                affected_apps.insert(fallback);
            }
            if report.id.is_empty() {
                self.records.clear();
            } else {
                self.records
                    .retain(|key, _| key != report.id && !is_descendant(key, report.id));
            }
            return affected_apps
                .into_iter()
                .map(|app| self.aggregate_for_app(&app))
                .collect();
        }

        // Capture any app groups that may lose records due to app reassignment
        // or LRU eviction so their downstream status is refreshed cleanly.
        let mut affected_apps = BTreeSet::new();
        if self.records.contains_key(report.id) {
            for key in self.records.keys() {
                if report.id.is_empty() || key == report.id || is_descendant(key, report.id) {
                    affected_apps.insert(self.resolve_app(key));
                }
            }
        }

        if !self.records.contains_key(report.id)
            && self.records.len() >= MAX_PROGRAM_STATUS_RECORDS
            && let Some(oldest_key) = self
                .records
                .iter()
                .min_by_key(|(_, record)| record.seq)
                .map(|(key, _)| key.clone())
        {
            affected_apps.insert(self.resolve_app(&oldest_key));
            self.records.remove(&oldest_key);
        }

        self.next_seq = self.next_seq.wrapping_add(1);
        let kind = (report.state == ProgramState::Blocked)
            .then_some(report.kind)
            .flatten();
        let progress = matches!(report.state, ProgramState::Working | ProgramState::Blocked)
            .then_some(report.progress)
            .flatten()
            .filter(|&p| p <= 100);
        // Spec: a report replaces its record completely; omitted fields do not
        // retain previous values.
        self.records.insert(
            report.id.to_string(),
            Record {
                state: report.state,
                kind,
                app: sanitize_external_text(report.app, MAX_APP_CHARS),
                title: sanitize_external_text(report.title, MAX_TITLE_CHARS),
                progress,
                message: sanitize_external_text(report.message, MAX_MESSAGE_CHARS),
                seq: self.next_seq,
            },
        );
        let target_app = self.resolve_app(report.id);
        affected_apps.insert(target_app.clone());

        // When the root record itself transitions to `idle` or `working`, prune
        // stale `working`/`blocked` child records in the same app group so an
        // un-cleared child dialog (e.g. after pressing Esc) cannot pin the pane
        // in `waiting` forever.
        if report.id.is_empty()
            && matches!(report.state, ProgramState::Idle | ProgramState::Working)
        {
            let stale_children: Vec<String> = self
                .records
                .iter()
                .filter(|(key, record)| {
                    !key.is_empty()
                        && matches!(record.state, ProgramState::Working | ProgramState::Blocked)
                        && self.resolve_app(key) == target_app
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in stale_children {
                self.records.remove(&key);
            }
        }

        affected_apps
            .into_iter()
            .map(|app| self.aggregate_for_app(&app))
            .collect()
    }

    fn resolve_app(&self, id: &str) -> String {
        let mut cursor = Some(id);
        while let Some(key) = cursor {
            if let Some(record) = self.records.get(key)
                && !record.app.is_empty()
            {
                return record.app.clone();
            }
            if key.is_empty() {
                break;
            }
            cursor = match key.rsplit_once('/') {
                Some((parent, _)) => Some(parent),
                None => Some(""),
            };
        }
        String::new()
    }

    fn aggregate_for_app(&self, target_app: &str) -> TermProgramStatus {
        let Some((_, winner)) = self
            .records
            .iter()
            .filter(|(id, _)| self.resolve_app(id) == target_app)
            .max_by_key(|(_, record)| (state_priority(record.state), record.seq))
        else {
            return TermProgramStatus {
                state: ProgramState::Clear,
                kind: None,
                app: target_app.to_string(),
                title: String::new(),
                progress: None,
                message: String::new(),
            };
        };
        TermProgramStatus {
            state: winner.state,
            kind: winner.kind,
            app: target_app.to_string(),
            title: winner.title.clone(),
            progress: winner.progress,
            message: winner.message.clone(),
        }
    }
}

fn state_priority(state: ProgramState) -> u8 {
    match state {
        ProgramState::Blocked => 5,
        ProgramState::Error => 4,
        ProgramState::Working => 3,
        ProgramState::Done => 2,
        ProgramState::Idle => 1,
        ProgramState::Clear => 0,
    }
}

fn is_descendant(key: &str, ancestor: &str) -> bool {
    key.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Sanitize untrusted OSC 7501 text (`app`, `title`, `message`) before it
/// leaves the terminal into notifications or UI chrome.
///
/// Strips C0/C1 control characters, Unicode bidi overrides/isolates/marks,
/// zero-width spaces/non-joiners, soft hyphens, invisible formatters, and
/// Unicode tag characters (while preserving `U+200D` ZWJ for compound emoji),
/// then collapses whitespace and clamps character length.
pub(super) fn sanitize_external_text(raw: &str, max_chars: usize) -> String {
    let stripped: String = raw.chars().filter(|&ch| !is_unsafe_invisible(ch)).collect();
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(max_chars).collect()
}

fn is_unsafe_invisible(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{00AD}' // soft hyphen
            | '\u{061C}' // Arabic letter mark
            | '\u{200B}'..='\u{200C}' // ZWSP / ZWNJ (keep U+200D ZWJ for emoji)
            | '\u{200E}'..='\u{200F}' // LRM / RLM
            | '\u{202A}'..='\u{202E}' // bidi embedding / override
            | '\u{2060}'..='\u{2064}' // word joiner / invisible math
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}' // BOM / ZWNBSP
            | '\u{E0000}'..='\u{E007F}' // Unicode tag characters
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        state: ProgramState,
        id: &'static str,
        app: &'static str,
    ) -> ProgramStatusReport<'static> {
        ProgramStatusReport {
            state,
            kind: None,
            progress: None,
            id,
            app,
            title: "",
            message: "",
        }
    }

    #[test]
    fn subtree_clear_removes_children_and_keeps_siblings() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Idle, "", "codex"));
        table.apply(report(ProgramState::Working, "build", ""));
        let blocked = table.apply(ProgramStatusReport {
            state: ProgramState::Blocked,
            kind: Some(ProgramBlockKind::Permission),
            progress: Some(40),
            id: "build/test",
            app: "",
            title: "Tests",
            message: "Approve?",
        });
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].state, ProgramState::Blocked);
        assert_eq!(blocked[0].kind, Some(ProgramBlockKind::Permission));
        assert_eq!(blocked[0].app, "codex", "inherits app from root ancestor");
        assert_eq!(blocked[0].progress, Some(40));

        // Sibling with shared prefix name ("builder") must NOT be cleared.
        table.apply(report(ProgramState::Working, "builder", ""));

        let after_clear = table.apply(report(ProgramState::Clear, "build", ""));
        assert_eq!(after_clear.len(), 1);
        assert_eq!(after_clear[0].state, ProgramState::Working);
        assert_eq!(after_clear[0].app, "codex");

        let cleared_all = table.apply(report(ProgramState::Clear, "", ""));
        assert_eq!(cleared_all.len(), 1);
        assert_eq!(cleared_all[0].state, ProgramState::Clear);
        assert_eq!(cleared_all[0].app, "codex");
    }

    #[test]
    fn stale_blocked_child_is_cleared_when_root_recovers_to_working_or_idle() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Working, "", "codex"));
        let blocked = table.apply(ProgramStatusReport {
            state: ProgramState::Blocked,
            kind: Some(ProgramBlockKind::Permission),
            progress: None,
            id: "s1",
            app: "",
            title: "Tool",
            message: "Approve?",
        });
        assert_eq!(blocked[0].state, ProgramState::Blocked);
        assert_eq!(blocked[0].app, "codex");

        // User cancels/continues and CLI reports root working without clearing "s1".
        let recovered = table.apply(report(ProgramState::Working, "", "codex"));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, ProgramState::Working);
        assert_eq!(recovered[0].app, "codex");

        // Same recovery to idle if another child blocked record is left behind.
        table.apply(report(ProgramState::Blocked, "s2", ""));
        let idle = table.apply(report(ProgramState::Idle, "", "codex"));
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].state, ProgramState::Idle);
    }

    #[test]
    fn prompt_start_clears_working_and_blocked_while_keeping_done_and_error() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Blocked, "s1", "antigravity"));
        table.apply(report(ProgramState::Working, "", "codex"));
        table.apply(report(ProgramState::Done, "task", "claude-code"));

        let updates = table.clear_on_prompt_start();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].app, "antigravity");
        assert_eq!(updates[0].state, ProgramState::Clear);
        assert_eq!(updates[1].app, "codex");
        assert_eq!(updates[1].state, ProgramState::Clear);
        assert_eq!(
            table.aggregate_for_app("claude-code").state,
            ProgramState::Done
        );
    }

    #[test]
    fn changing_record_app_and_lru_eviction_emit_clear_for_previous_app_group() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Working, "r1", "codex"));
        let updates = table.apply(report(ProgramState::Working, "r1", "antigravity"));
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].app, "antigravity");
        assert_eq!(updates[0].state, ProgramState::Working);
        assert_eq!(updates[1].app, "codex");
        assert_eq!(updates[1].state, ProgramState::Clear);

        // Fill table so "r1" (the sole antigravity record) is evicted by LRU.
        let mut evicted_antigravity = false;
        for idx in 0..MAX_PROGRAM_STATUS_RECORDS {
            let id = format!("codex-{idx}");
            let batch = table.apply(ProgramStatusReport {
                state: ProgramState::Working,
                kind: None,
                progress: None,
                id: &id,
                app: "codex",
                title: "",
                message: "",
            });
            if batch
                .iter()
                .any(|s| s.app == "antigravity" && s.state == ProgramState::Clear)
            {
                evicted_antigravity = true;
            }
        }
        assert!(evicted_antigravity);
    }

    #[test]
    fn nested_foreign_app_does_not_mask_agent_status() {
        let mut table = ProgramStatusTable::default();
        let codex_working = table.apply(report(ProgramState::Working, "", "codex"));
        assert_eq!(codex_working[0].state, ProgramState::Working);
        assert_eq!(codex_working[0].app, "codex");

        // Nested terraform emits blocked under its own id/app; it aggregates
        // only in the terraform group and never masks codex.
        let tf_blocked = table.apply(ProgramStatusReport {
            state: ProgramState::Blocked,
            kind: Some(ProgramBlockKind::Permission),
            progress: None,
            id: "tf",
            app: "terraform",
            title: "Plan",
            message: "Apply?",
        });
        assert_eq!(tf_blocked.len(), 1);
        assert_eq!(tf_blocked[0].app, "terraform");
        assert_eq!(tf_blocked[0].state, ProgramState::Blocked);

        // Codex's own aggregate remains working/codex.
        let codex_child_done = table.apply(report(ProgramState::Done, "sub", ""));
        assert_eq!(codex_child_done.len(), 1);
        assert_eq!(codex_child_done[0].app, "codex");
        assert_eq!(codex_child_done[0].state, ProgramState::Working);

        // Clearing terraform only emits Clear for terraform, leaving codex intact.
        let tf_cleared = table.apply(report(ProgramState::Clear, "tf", ""));
        assert_eq!(tf_cleared.len(), 1);
        assert_eq!(tf_cleared[0].app, "terraform");
        assert_eq!(tf_cleared[0].state, ProgramState::Clear);
        assert_eq!(
            table.aggregate_for_app("codex").state,
            ProgramState::Working
        );
    }

    #[test]
    fn nearest_ancestor_app_and_priority_aggregation() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Idle, "", "codex"));
        table.apply(report(ProgramState::Working, "sub", "antigravity"));
        let status = table.apply(report(ProgramState::Done, "sub/child", ""));
        assert_eq!(
            status[0].state,
            ProgramState::Working,
            "working outranks done"
        );
        assert_eq!(status[0].app, "antigravity");

        let err = table.apply(report(ProgramState::Error, "sub/child", ""));
        assert_eq!(err[0].state, ProgramState::Error, "error outranks working");
        assert_eq!(err[0].app, "antigravity", "inherits nearest ancestor app");
    }

    #[test]
    fn clear_all_drops_stale_records_after_process_exit() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Blocked, "sub/dialog", "antigravity"));
        table.clear_all();
        let next = table.apply(report(ProgramState::Working, "", "codex"));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].state, ProgramState::Working);
        assert_eq!(next[0].app, "codex");
    }

    #[test]
    fn capacity_evicts_oldest_updated_record() {
        let mut table = ProgramStatusTable::default();
        table.apply(report(ProgramState::Blocked, "old-blocked", "codex"));
        for idx in 0..MAX_PROGRAM_STATUS_RECORDS {
            let id = format!("task-{idx}");
            table.apply(ProgramStatusReport {
                state: ProgramState::Working,
                kind: None,
                progress: None,
                id: &id,
                app: "codex",
                title: "",
                message: "",
            });
        }
        assert_eq!(table.records.len(), MAX_PROGRAM_STATUS_RECORDS);
        assert!(!table.records.contains_key("old-blocked"));
    }

    #[test]
    fn sanitize_strips_bidi_overrides_zero_width_and_control_chars() {
        let dirty = "  Trust\u{202E} this\u{200B}\u{200C} folder?\x07\u{2066} \u{FEFF}ok  ";
        assert_eq!(sanitize_external_text(dirty, 100), "Trust this folder? ok");
    }
}
