//! Live agent status tracking, title resolution, and background shell-agent watcher.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::agent::launch::PaneLaunch;
use crate::agent::status::clean_osc_title;
use crate::agent::{AgentState, StatusSignal, StatusSource};
use crate::app::Workspace;
use crate::app::layout::PaneId;
use crate::term::{ProgramBlockKind, ProgramState, TermNotification, TermProgramStatus};
use gpui::*;

use super::KumuApp;
use super::terminal::TerminalEvent;

const OSC7501_ALERT_DEDUPE: Duration = Duration::from_secs(2);

/// The name a pane's agent shows in the UI, for a pane whose program has not
/// set a title yet.
pub(super) fn agent_display_name(id: &str) -> Option<String> {
    crate::agent::AGENTS
        .iter()
        .find(|agent| agent.id == id)
        .map(|agent| agent.name.to_string())
}

/// What the sidebar, pane header and tab strip know about a live agent.
#[derive(Clone, Debug)]
pub(super) struct AgentEntry {
    /// Active state, or `None` when the agent is alive and idle/ready.
    pub(super) state: Option<AgentState>,
    /// Authoritative session title reported via `kumu-title:<title>`.
    pub(super) session_title: Option<String>,
    /// First prompt reported via `kumu-title:prompt:<text>`.
    pub(super) prompt_title: Option<String>,
    /// Cleaned OSC 0/2 window title (`snapshot.title`).
    pub(super) osc_title: Option<String>,
    /// Agent id (e.g. `"claude"`, `"opencode"`).
    pub(super) agent: String,
    /// Whether the background shell-process watcher has observed this agent's
    /// process in the pane's process tree (so it can safely prune it on exit).
    pub(super) process_tracked: bool,
    /// A status change arrived while this pane was not being looked at. The
    /// sidebar and tab markers keep it visible until the user views the pane.
    pub(super) unseen: bool,
    /// Whether OSC 7501 has reported status for this agent instance.
    pub(super) osc7501_active: bool,
    /// Last OSC 7501 transition key observed, used to fire attention on state,
    /// block-kind, or blocked-prompt changes without spamming progress ticks.
    pub(super) last_osc7501_key: Option<Osc7501AlertKey>,
    /// Whether Kumu's registered integration (extension/hook) has reported
    /// status for this agent instance.
    pub(super) integration_active: bool,
    /// Most recent OSC 7501 attention notification, for 2-second deduplication.
    pub(super) last_osc7501_alert: Option<(Instant, TermNotification)>,
}

/// Transition key for OSC 7501 attention notifications.
///
/// Tracks `state`, `kind`, and (for `Blocked`) `(title, message)` so moving
/// from `Blocked(Permission)` to `Blocked(Question)` or a new approval prompt
/// raises a fresh alert, while progress-only ticks on the same prompt do not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Osc7501AlertKey {
    state: ProgramState,
    kind: Option<ProgramBlockKind>,
    prompt_detail: Option<(String, String)>,
}

impl Osc7501AlertKey {
    fn from_status(status: &TermProgramStatus) -> Self {
        let prompt_detail = (status.state == ProgramState::Blocked)
            .then(|| (status.title.clone(), status.message.clone()));
        Self {
            state: status.state,
            kind: (status.state == ProgramState::Blocked)
                .then_some(status.kind)
                .flatten(),
            prompt_detail,
        }
    }
}

/// Resolve which known agent an OSC 7501 status report belongs to.
///
/// - When `reported_app` is non-empty, it must match a known agent via
///   [`crate::agent::id_for`] so nested tools (`cargo`, `terraform`, `brew`)
///   never corrupt agent status.
/// - A pane explicitly launched as an agent (`launched = Some(L)`) never
///   switches to a different agent from a nested OSC 7501 report.
/// - A shell pane with an already-active agent (`existing_active = true`) does
///   not switch agents mid-turn from a nested OSC 7501 report; exit or
///   `reconcile_shell_agents` resets the pane first.
fn resolve_program_status_agent(
    launched: Option<&str>,
    existing: Option<&str>,
    existing_active: bool,
    reported_app: &str,
) -> Option<String> {
    if !reported_app.is_empty() {
        let matched = crate::agent::id_for(reported_app)?;
        if let Some(launched) = launched {
            return (launched == matched).then(|| matched.to_string());
        }
        if existing_active && existing.is_some_and(|current| current != matched) {
            return None;
        }
        return Some(matched.to_string());
    }
    launched.or(existing).map(str::to_string)
}

/// Whether an OSC 7501 `Clear` should remove an untracked shell pane entry so
/// agents invisible to the local process watcher (e.g. over SSH/tmux/wrappers)
/// do not stay pinned as idle entries in the sidebar after exit.
fn should_remove_shell_entry_on_clear(
    is_shell: bool,
    is_launched_agent: bool,
    process_tracked: bool,
) -> bool {
    is_shell && !is_launched_agent && !process_tracked
}

/// Log an unrecognized OSC 7501 `app` on an explicitly launched agent pane once
/// per unique `(launched_agent, app)` pair to help diagnose renamed CLIs or
/// custom `app` configs (such as Pi's `piConfigName`) without flooding logs.
fn log_unknown_osc7501_app(launched: &str, app: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
    let key = format!("{launched}:{app}");
    if guard.len() < 64 && guard.insert(key) {
        eprintln!(
            "[agent] ignoring OSC 7501 report with unrecognized app {app:?} on {launched:?} pane"
        );
    }
}

/// Whether an OSC 7501 report establishes OSC 7501 authority for the pane.
///
/// `Clear` reports and anonymous reports without an `app` must not claim
/// authority over a pane's Hooks (for example, a nested tool emitting
/// `state=clear` inside a Cursor CLI pane).
fn should_activate_osc7501(status: &TermProgramStatus) -> bool {
    status.state != ProgramState::Clear && !status.app.is_empty()
}

/// Single-source-of-truth authority check per agent capability:
/// - In-process extension agents (`opencode`, `pi`, `omp`): Integration > OSC 7501.
/// - External hook agents (`codex`, `claude`, `agy`, `grok`, etc.): OSC 7501 > Hooks.
fn accepts_status(
    agent_id: &str,
    source: StatusSource,
    osc7501_active: bool,
    integration_active: bool,
) -> bool {
    if crate::agent::prefers_integration_status(agent_id) {
        match source {
            StatusSource::Integration => true,
            StatusSource::Osc7501 => !integration_active,
        }
    } else {
        match source {
            StatusSource::Osc7501 => true,
            StatusSource::Integration => !osc7501_active,
        }
    }
}

/// Suppress duplicate legacy Hook OSC 777 text alerts once OSC 7501 is
/// authoritative for a hook-based agent on the same pane, while leaving
/// unrelated custom OSC 777 notifications untouched.
fn should_suppress_hook_attention(
    agent_id: &str,
    osc7501_active: bool,
    notification: &TermNotification,
) -> bool {
    osc7501_active
        && !crate::agent::prefers_integration_status(agent_id)
        && crate::agent::id_for(&notification.title) == Some(agent_id)
        && crate::agent::integration::is_hook_notification_body(&notification.body)
}

fn should_alert_osc7501(previous: Option<&Osc7501AlertKey>, current: &Osc7501AlertKey) -> bool {
    previous != Some(current)
}

fn osc7501_status_signal(state: ProgramState) -> StatusSignal {
    match state {
        ProgramState::Working => StatusSignal::Active(AgentState::Running),
        ProgramState::Blocked => StatusSignal::Active(AgentState::Waiting),
        ProgramState::Done => StatusSignal::Active(AgentState::Completed),
        ProgramState::Error => StatusSignal::Active(AgentState::Failed),
        ProgramState::Idle | ProgramState::Clear => StatusSignal::Idle,
    }
}

fn osc7501_attention_notification(
    agent_id: &str,
    status: &TermProgramStatus,
) -> Option<TermNotification> {
    let default_body = match (status.state, status.kind) {
        (ProgramState::Blocked, Some(ProgramBlockKind::Permission)) => "需要你确认操作权限",
        (ProgramState::Blocked, Some(ProgramBlockKind::Question)) => "需要你输入、确认或选择",
        (ProgramState::Blocked, Some(ProgramBlockKind::Auth)) => "需要你完成认证",
        (ProgramState::Blocked, None) => "需要你的输入或确认",
        (ProgramState::Done, _) => "本轮任务已结束",
        (ProgramState::Error, _) => "任务执行失败",
        (ProgramState::Idle | ProgramState::Working | ProgramState::Clear, _) => return None,
    };
    let display = agent_display_name(agent_id).unwrap_or_else(|| agent_id.to_string());
    let title = if !status.title.is_empty()
        && !status.title.eq_ignore_ascii_case(&display)
        && !status.title.eq_ignore_ascii_case(agent_id)
    {
        format!("{display} · {}", status.title)
    } else {
        display
    };
    let body = if !status.message.is_empty() {
        status.message.clone()
    } else {
        default_body.to_string()
    };
    Some(TermNotification { title, body })
}

/// Decide whether a status transition leaves the pane needing the user's
/// attention. `existing` is kept for a repeated signal so a marker the user
/// already acknowledged does not come back, and a non-actionable state (idle,
/// running) clears it.
fn unseen_after_status(
    existing: bool,
    previous: Option<AgentState>,
    current: Option<AgentState>,
    attended: bool,
) -> bool {
    match current {
        Some(AgentState::Waiting | AgentState::Completed | AgentState::Failed) => {
            if previous == current {
                existing
            } else {
                !attended
            }
        }
        None | Some(AgentState::Running) => false,
    }
}

impl AgentEntry {
    pub(super) fn idle(agent: String) -> Self {
        Self {
            state: None,
            session_title: None,
            prompt_title: None,
            osc_title: None,
            agent,
            process_tracked: false,
            unseen: false,
            osc7501_active: false,
            last_osc7501_key: None,
            integration_active: false,
            last_osc7501_alert: None,
        }
    }

    /// Reset transient status, titles, and authority flags when a different
    /// agent takes over the same shell pane.
    ///
    /// Keeps `process_tracked` untouched: it tracks whether the shell process
    /// watcher has observed a child agent on this pane, which is orthogonal to
    /// agent identity updates from `Session` or OSC status events.
    pub(super) fn switch_agent(&mut self, agent: String) {
        self.agent = agent;
        self.session_title = None;
        self.prompt_title = None;
        self.osc_title = None;
        self.state = None;
        self.unseen = false;
        self.osc7501_active = false;
        self.last_osc7501_key = None;
        self.integration_active = false;
        self.last_osc7501_alert = None;
    }

    /// Title priority: reported session title > first prompt > OSC 0/2 title > agent name.
    pub(super) fn display_title(&self) -> String {
        self.session_title
            .as_deref()
            .or(self.prompt_title.as_deref())
            .or(self.osc_title.as_deref())
            .map(str::to_string)
            .or_else(|| agent_display_name(&self.agent))
            .unwrap_or_else(|| self.agent.clone())
    }
}

impl KumuApp {
    pub(super) fn on_terminal_event(
        &mut self,
        pane: PaneId,
        ev: &TerminalEvent,
        cx: &mut Context<Self>,
    ) {
        match ev {
            TerminalEvent::Activated => self.focus_pane_from_user(pane, cx),
            TerminalEvent::Interacted => {
                if self.acknowledge_pane(pane, cx) {
                    cx.notify();
                }
            }
            TerminalEvent::ChromeChanged => cx.notify(),
            TerminalEvent::ContextMenu { position } => {
                self.open_terminal_context_menu(pane, *position, cx);
            }
        }
    }

    /// The agent id a pane was launched with, or `None` for a shell.
    fn pane_agent_id(&self, pane: PaneId) -> Option<String> {
        match self.launches.get(&pane) {
            Some(PaneLaunch::Agent { agent, .. }) => Some(agent.clone()),
            _ => None,
        }
    }

    /// Ensure an `AgentEntry` exists for `pane`, seeding its OSC 0/2 title if
    /// the terminal already set one.
    fn ensure_agent_entry(
        &mut self,
        pane: PaneId,
        reported_agent: &str,
        cx: &Context<Self>,
    ) -> &mut AgentEntry {
        let launched = self.pane_agent_id(pane);
        let agent = crate::agent::id_for(reported_agent)
            .map(str::to_string)
            .or(launched)
            .unwrap_or_else(|| reported_agent.to_string());
        let osc_title = self
            .workspace
            .read(cx)
            .pane_title(pane.0)
            .and_then(|title| clean_osc_title(&title, &agent));
        let entry = self.agent_status.entry(pane).or_insert_with(|| {
            let mut entry = AgentEntry::idle(agent.clone());
            entry.osc_title = osc_title;
            entry
        });
        if !agent.is_empty() && entry.agent != agent {
            entry.switch_agent(agent);
        }
        entry
    }

    /// Whether an OSC 777 text notification for `pane` is a duplicate legacy
    /// Hook alert already covered by OSC 7501.
    pub(super) fn suppresses_attention(
        &self,
        pane: PaneId,
        notification: &TermNotification,
    ) -> bool {
        self.agent_status.get(&pane).is_some_and(|entry| {
            should_suppress_hook_attention(&entry.agent, entry.osc7501_active, notification)
        })
    }

    /// Record a status signal an agent reported on its silent OSC 777 channel.
    pub(super) fn note_agent_status(
        &mut self,
        pane: PaneId,
        project: i64,
        agent: &str,
        signal: StatusSignal,
        cx: &mut Context<Self>,
    ) {
        // A stale process must not write into a pane that now belongs to
        // another project.
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        if signal == StatusSignal::Exited {
            if self.workspace.read(cx).pane_is_shell(pane.0) == Some(true)
                && self.agent_status.remove(&pane).is_some()
            {
                self.workspace
                    .read(cx)
                    .reset_terminal_program_status(pane.0);
                self.sync_pane_agent_status(pane, cx);
                cx.notify();
            }
            return;
        }
        let resolved_agent = crate::agent::id_for(agent)
            .map(str::to_string)
            .or_else(|| self.pane_agent_id(pane))
            .unwrap_or_else(|| agent.to_string());
        if let Some(existing) = self.agent_status.get(&pane)
            && (resolved_agent.is_empty() || existing.agent == resolved_agent)
            && !accepts_status(
                &existing.agent,
                StatusSource::Integration,
                existing.osc7501_active,
                existing.integration_active,
            )
        {
            return;
        }
        // Whether the user is looking at this pane decides whether the status
        // becomes an unseen marker. Read it before borrowing the entry.
        let attended = self.pane_is_attended(pane);
        let entry = self.ensure_agent_entry(pane, agent, cx);
        entry.integration_active = true;
        let previous = entry.state;
        match signal {
            StatusSignal::Active(state) => entry.state = Some(state),
            StatusSignal::Resume => {
                if entry.state == Some(AgentState::Waiting) {
                    entry.state = Some(AgentState::Running);
                }
            }
            StatusSignal::Idle => entry.state = None,
            StatusSignal::Exited => unreachable!(),
        }
        entry.unseen = unseen_after_status(entry.unseen, previous, entry.state, attended);
        self.sync_pane_agent_status(pane, cx);
        cx.notify();
    }

    /// Record an aggregated OSC 7501 program status report for `pane`.
    pub(super) fn note_program_status(
        &mut self,
        terminal: u64,
        pane: PaneId,
        project: i64,
        status: &TermProgramStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        let launched = self.pane_agent_id(pane);
        let existing_info = self.agent_status.get(&pane).map(|entry| {
            (
                entry.agent.as_str(),
                entry.state.is_some() || entry.osc7501_active || entry.integration_active,
            )
        });
        let Some(agent_id) = resolve_program_status_agent(
            launched.as_deref(),
            existing_info.map(|(agent, _)| agent),
            existing_info.is_some_and(|(_, active)| active),
            &status.app,
        ) else {
            if let Some(launched_agent) = launched.as_deref()
                && !status.app.is_empty()
                && crate::agent::id_for(&status.app).is_none()
            {
                log_unknown_osc7501_app(launched_agent, &status.app);
            }
            return;
        };
        if status.state == ProgramState::Clear {
            let Some(existing) = self
                .agent_status
                .get(&pane)
                .filter(|entry| entry.agent == agent_id && entry.osc7501_active)
            else {
                return;
            };
            let is_shell = self.workspace.read(cx).pane_is_shell(pane.0) == Some(true);
            if should_remove_shell_entry_on_clear(
                is_shell,
                launched.is_some(),
                existing.process_tracked,
            ) {
                self.agent_status.remove(&pane);
                self.workspace
                    .read(cx)
                    .reset_terminal_program_status(pane.0);
                self.sync_pane_agent_status(pane, cx);
                cx.notify();
                return;
            }
        }
        if let Some(existing) = self.agent_status.get(&pane)
            && existing.agent == agent_id
            && !accepts_status(
                &agent_id,
                StatusSource::Osc7501,
                existing.osc7501_active,
                existing.integration_active,
            )
        {
            return;
        }
        let attended = self.pane_is_attended(pane);
        let entry = self.ensure_agent_entry(pane, &agent_id, cx);
        if should_activate_osc7501(status) {
            entry.osc7501_active = true;
        }
        let next_key = Osc7501AlertKey::from_status(status);
        let should_alert = should_alert_osc7501(entry.last_osc7501_key.as_ref(), &next_key);
        entry.last_osc7501_key = Some(next_key);

        let signal = osc7501_status_signal(status.state);
        let previous = entry.state;
        match signal {
            StatusSignal::Active(state) => entry.state = Some(state),
            StatusSignal::Idle => entry.state = None,
            StatusSignal::Resume | StatusSignal::Exited => unreachable!(),
        }
        entry.unseen = unseen_after_status(entry.unseen, previous, entry.state, attended);

        let alert = if should_alert {
            osc7501_attention_notification(&agent_id, status).and_then(|notification| {
                let now = Instant::now();
                let duplicate =
                    entry
                        .last_osc7501_alert
                        .as_ref()
                        .is_some_and(|(last, previous_alert)| {
                            now.duration_since(*last) < OSC7501_ALERT_DEDUPE
                                && previous_alert == &notification
                        });
                if duplicate {
                    None
                } else {
                    entry.last_osc7501_alert = Some((now, notification.clone()));
                    Some(notification)
                }
            })
        } else {
            None
        };

        self.sync_pane_agent_status(pane, cx);
        if let Some(notification) = alert {
            self.raise_attention(terminal, project, &notification, window, cx);
        }
        cx.notify();
    }

    /// Record a session or first-prompt title reported on `kumu-title:`.
    pub(super) fn note_agent_title(
        &mut self,
        pane: PaneId,
        project: i64,
        agent: &str,
        title: &str,
        is_prompt: bool,
        cx: &mut Context<Self>,
    ) {
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        let entry = self.ensure_agent_entry(pane, agent, cx);
        if !is_prompt {
            entry.session_title = Some(title.to_string());
        } else if entry.prompt_title.is_none() {
            entry.prompt_title = Some(title.to_string());
        }
        cx.notify();
    }

    /// Keep a live agent's OSC 0/2 fallback title up to date when the terminal
    /// changes its window title.
    pub(super) fn note_pane_title(
        &mut self,
        pane: PaneId,
        project: i64,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        if self.pane_tab(pane).map(|(owner, _)| owner) != Some(project) {
            return;
        }
        let Some(entry) = self.agent_status.get_mut(&pane) else {
            return;
        };
        let next = clean_osc_title(title, &entry.agent);
        if next != entry.osc_title {
            entry.osc_title = next;
            cx.notify();
        }
    }

    /// Whether the user can actually see this pane right now: the window has
    /// focus, no full-window overlay covers it, its tab is selected, it is not
    /// hidden behind a zoomed sibling, and it holds keyboard focus.
    fn pane_is_attended(&self, pane: PaneId) -> bool {
        self.window_active
            && self.active_sheet.is_none()
            && self.active_dialog.is_none()
            && self.active_palette.is_none()
            && self.active_agent_picker.is_none()
            && self.pane_is_visible(pane)
            && self
                .active_zoomed_pane()
                .is_none_or(|zoomed| zoomed == pane)
            && self.focused == pane
    }

    /// Clear the unseen marker for a pane whose contents are now on screen.
    /// Returns whether a marker was actually cleared.
    pub(super) fn acknowledge_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.agent_status.get_mut(&pane) else {
            return false;
        };
        if !entry.unseen {
            return false;
        }
        entry.unseen = false;
        self.sync_pane_agent_status(pane, cx);
        true
    }

    /// Push the active state the pane's header should draw (`None` for idle
    /// agents and plain shells).
    pub(super) fn sync_pane_agent_status(&self, pane: PaneId, cx: &mut Context<Self>) {
        let (state, unseen) = self
            .agent_status
            .get(&pane)
            .map_or((None, false), |entry| (entry.state, entry.unseen));
        if let Some(view) = self.panes.get(&pane) {
            view.update(cx, |view, cx| view.set_agent_status(state, unseen, cx));
        }
    }

    /// Periodically inspect live shell panes on a background thread so manually
    /// launched agents appear immediately (even before the first prompt) and
    /// leave the sidebar when their process exits back to the shell.
    pub(super) fn start_shell_agent_watcher(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let Ok(shells) =
                    this.update(cx, |app, cx| app.workspace.read(cx).live_shell_panes())
                else {
                    break;
                };
                if shells.is_empty() {
                    continue;
                }
                let detected = cx
                    .background_executor()
                    .spawn(async move { Workspace::detect_shell_agents(&shells) })
                    .await;
                let Some(detected) = detected else {
                    continue;
                };
                if this
                    .update(cx, |app, cx| app.reconcile_shell_agents(detected, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn reconcile_shell_agents(
        &mut self,
        detected: Vec<(u64, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        let mut removed_panes = Vec::new();
        let mut switched_panes = Vec::new();
        for (pane_raw, detected_agent) in detected {
            let pane = PaneId(pane_raw);
            if self.workspace.read(cx).pane_is_shell(pane_raw) != Some(true) {
                continue;
            }
            match detected_agent {
                Some(agent) => {
                    let osc_title = self
                        .workspace
                        .read(cx)
                        .pane_title(pane_raw)
                        .and_then(|title| clean_osc_title(&title, &agent));
                    if let Some(entry) = self.agent_status.get_mut(&pane) {
                        if entry.agent != agent {
                            entry.switch_agent(agent);
                            entry.osc_title = osc_title;
                            switched_panes.push(pane);
                            changed = true;
                        }
                        entry.process_tracked = true;
                    } else {
                        let mut entry = AgentEntry::idle(agent);
                        entry.osc_title = osc_title;
                        entry.process_tracked = true;
                        self.agent_status.insert(pane, entry);
                        changed = true;
                    }
                }
                None => {
                    if self
                        .agent_status
                        .get(&pane)
                        .is_some_and(|entry| entry.process_tracked)
                    {
                        self.agent_status.remove(&pane);
                        removed_panes.push(pane);
                        changed = true;
                    }
                }
            }
        }
        if !switched_panes.is_empty() {
            let workspace = self.workspace.read(cx);
            for pane in &switched_panes {
                workspace.reset_terminal_program_status(pane.0);
            }
            for pane in switched_panes {
                self.sync_pane_agent_status(pane, cx);
            }
        }
        if !removed_panes.is_empty() {
            let workspace = self.workspace.read(cx);
            for pane in &removed_panes {
                workspace.reset_terminal_program_status(pane.0);
            }
            for pane in removed_panes {
                self.sync_pane_agent_status(pane, cx);
            }
        }
        if changed {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentEntry, Osc7501AlertKey, accepts_status, osc7501_attention_notification,
        osc7501_status_signal, resolve_program_status_agent, should_activate_osc7501,
        should_alert_osc7501, should_remove_shell_entry_on_clear, should_suppress_hook_attention,
        unseen_after_status,
    };
    use crate::agent::{AgentState, StatusSignal, StatusSource};
    use crate::term::{ProgramBlockKind, ProgramState, TermNotification, TermProgramStatus};

    #[test]
    fn osc7501_filters_non_agent_commands_and_prevents_cross_agent_hijacking() {
        // Explicit non-agent app is ignored even inside an agent pane or tracked shell.
        assert_eq!(
            resolve_program_status_agent(Some("claude"), Some("claude"), true, "cargo"),
            None
        );
        assert_eq!(
            resolve_program_status_agent(None, None, false, "terraform"),
            None
        );
        // Plain shell with empty app is ignored.
        assert_eq!(resolve_program_status_agent(None, None, false, ""), None);

        // A launched agent pane never switches to a different agent from a nested app.
        assert_eq!(
            resolve_program_status_agent(Some("codex"), Some("codex"), true, "claude-code"),
            None
        );
        // An active agent in a shell pane is not hijacked mid-turn by a nested agent.
        assert_eq!(
            resolve_program_status_agent(None, Some("codex"), true, "claude-code"),
            None
        );
        // Once the shell entry is idle/inactive, a newly launched agent can adopt it.
        assert_eq!(
            resolve_program_status_agent(None, Some("codex"), false, "claude-code").as_deref(),
            Some("claude")
        );

        // Known agent app works in a plain shell pane before the process watcher runs.
        assert_eq!(
            resolve_program_status_agent(None, None, false, "antigravity").as_deref(),
            Some("agy")
        );
        // Empty app falls back to pane launch or process-watcher entry.
        assert_eq!(
            resolve_program_status_agent(Some("codex"), None, false, "").as_deref(),
            Some("codex")
        );
        assert_eq!(
            resolve_program_status_agent(None, Some("claude"), false, "").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn clear_and_empty_app_reports_do_not_claim_osc7501_authority() {
        let clear = TermProgramStatus {
            state: ProgramState::Clear,
            kind: None,
            app: "codex".into(),
            title: String::new(),
            progress: None,
            message: String::new(),
        };
        assert!(!should_activate_osc7501(&clear));

        let anonymous_working = TermProgramStatus {
            state: ProgramState::Working,
            kind: None,
            app: String::new(),
            title: String::new(),
            progress: None,
            message: String::new(),
        };
        assert!(!should_activate_osc7501(&anonymous_working));

        let codex_working = TermProgramStatus {
            app: "codex".into(),
            ..anonymous_working
        };
        assert!(should_activate_osc7501(&codex_working));
    }

    #[test]
    fn authority_rules_split_extension_agents_and_hook_agents() {
        // Hook-based agents (codex/claude/agy/grok): OSC 7501 > Hooks.
        assert!(accepts_status("codex", StatusSource::Osc7501, false, true));
        assert!(!accepts_status(
            "codex",
            StatusSource::Integration,
            true,
            false
        ));
        assert!(accepts_status(
            "codex",
            StatusSource::Integration,
            false,
            false
        ));

        let hook_done = TermNotification {
            title: "codex".into(),
            body: "本轮任务已结束".into(),
        };
        let hook_perm = TermNotification {
            title: "antigravity".into(),
            body: "需要你确认操作权限".into(),
        };
        let custom_note = TermNotification {
            title: "codex".into(),
            body: "Deploy preview ready at https://example.com".into(),
        };
        let other_tool = TermNotification {
            title: "other-tool".into(),
            body: "本轮任务已结束".into(),
        };
        assert!(should_suppress_hook_attention("codex", true, &hook_done));
        assert!(should_suppress_hook_attention("agy", true, &hook_perm));
        assert!(!should_suppress_hook_attention("codex", true, &custom_note));
        assert!(!should_suppress_hook_attention("codex", true, &other_tool));

        // Native extension agents (opencode/pi/omp): Integration > OSC 7501.
        assert!(accepts_status(
            "opencode",
            StatusSource::Integration,
            true,
            false
        ));
        assert!(!accepts_status(
            "opencode",
            StatusSource::Osc7501,
            false,
            true
        ));
        // Without the extension active, OSC 7501 drives status out of the box.
        assert!(accepts_status("pi", StatusSource::Osc7501, false, false));
        let oc_note = TermNotification {
            title: "OpenCode".into(),
            body: "本轮任务已结束".into(),
        };
        assert!(!should_suppress_hook_attention("opencode", true, &oc_note));
    }

    #[test]
    fn blocked_kind_and_prompt_changes_trigger_new_alerts_while_progress_ticks_do_not() {
        let perm = TermProgramStatus {
            state: ProgramState::Blocked,
            kind: Some(ProgramBlockKind::Permission),
            app: "antigravity".into(),
            title: "Tool".into(),
            progress: Some(10),
            message: "Trust this folder?".into(),
        };
        let perm_progress = TermProgramStatus {
            progress: Some(50),
            ..perm.clone()
        };
        let question = TermProgramStatus {
            kind: Some(ProgramBlockKind::Question),
            message: "Pick an option".into(),
            ..perm.clone()
        };
        let auth = TermProgramStatus {
            kind: Some(ProgramBlockKind::Auth),
            message: "Sign in required".into(),
            ..perm.clone()
        };

        let k_perm = Osc7501AlertKey::from_status(&perm);
        let k_perm_progress = Osc7501AlertKey::from_status(&perm_progress);
        let k_question = Osc7501AlertKey::from_status(&question);
        let k_auth = Osc7501AlertKey::from_status(&auth);

        assert!(should_alert_osc7501(None, &k_perm));
        assert!(!should_alert_osc7501(Some(&k_perm), &k_perm_progress));
        assert!(should_alert_osc7501(Some(&k_perm), &k_question));
        assert!(should_alert_osc7501(Some(&k_question), &k_auth));
    }

    #[test]
    fn switch_agent_clears_titles_and_authority_while_preserving_process_tracking() {
        let mut entry = AgentEntry::idle("agy".into());
        entry.process_tracked = true;
        entry.osc7501_active = true;
        entry.integration_active = true;
        entry.osc_title = Some("Old task title".into());
        entry.session_title = Some("Old session".into());
        entry.prompt_title = Some("Old prompt".into());
        entry.state = Some(AgentState::Waiting);

        entry.switch_agent("codex".into());
        assert_eq!(entry.agent, "codex");
        assert!(entry.process_tracked);
        assert!(!entry.osc7501_active);
        assert!(!entry.integration_active);
        assert_eq!(entry.osc_title, None);
        assert_eq!(entry.session_title, None);
        assert_eq!(entry.prompt_title, None);
        assert_eq!(entry.state, None);
        assert!(entry.last_osc7501_key.is_none());
    }

    #[test]
    fn untracked_shell_entry_is_removed_on_osc7501_clear() {
        assert!(should_remove_shell_entry_on_clear(true, false, false));
        assert!(!should_remove_shell_entry_on_clear(true, false, true));
        assert!(!should_remove_shell_entry_on_clear(true, true, false));
        assert!(!should_remove_shell_entry_on_clear(false, false, false));
    }

    #[test]
    fn osc7501_maps_states_and_attention_copy() {
        assert_eq!(
            osc7501_status_signal(ProgramState::Working),
            StatusSignal::Active(AgentState::Running)
        );
        assert_eq!(
            osc7501_status_signal(ProgramState::Blocked),
            StatusSignal::Active(AgentState::Waiting)
        );
        assert_eq!(
            osc7501_status_signal(ProgramState::Done),
            StatusSignal::Active(AgentState::Completed)
        );
        assert_eq!(
            osc7501_status_signal(ProgramState::Error),
            StatusSignal::Active(AgentState::Failed)
        );
        assert_eq!(
            osc7501_status_signal(ProgramState::Idle),
            StatusSignal::Idle
        );
        assert_eq!(
            osc7501_status_signal(ProgramState::Clear),
            StatusSignal::Idle
        );

        let blocked = TermProgramStatus {
            state: ProgramState::Blocked,
            kind: Some(ProgramBlockKind::Permission),
            app: "antigravity".into(),
            title: String::new(),
            progress: None,
            message: String::new(),
        };
        let alert = osc7501_attention_notification("agy", &blocked).expect("blocked alert");
        assert_eq!(alert.title, "Antigravity CLI");
        assert_eq!(alert.body, "需要你确认操作权限");

        let custom = TermProgramStatus {
            state: ProgramState::Blocked,
            kind: Some(ProgramBlockKind::Auth),
            app: "antigravity".into(),
            title: "Login".into(),
            progress: None,
            message: "Sign in required".into(),
        };
        let alert = osc7501_attention_notification("agy", &custom).expect("auth alert");
        assert_eq!(alert.title, "Antigravity CLI · Login");
        assert_eq!(alert.body, "Sign in required");

        let working = TermProgramStatus {
            state: ProgramState::Working,
            kind: None,
            app: "codex".into(),
            title: String::new(),
            progress: Some(50),
            message: String::new(),
        };
        assert_eq!(osc7501_attention_notification("codex", &working), None);
    }

    #[test]
    fn status_changes_mark_unseen_only_when_the_pane_is_not_being_watched() {
        // A transition to a state that wants the user's attention marks the
        // pane when it is off screen or not focused.
        assert!(unseen_after_status(
            false,
            Some(AgentState::Running),
            Some(AgentState::Waiting),
            false,
        ));
        assert!(unseen_after_status(
            false,
            Some(AgentState::Running),
            Some(AgentState::Completed),
            false,
        ));
        assert!(unseen_after_status(
            false,
            Some(AgentState::Running),
            Some(AgentState::Failed),
            false,
        ));

        // The same transition on the focused pane is already visible.
        assert!(!unseen_after_status(
            false,
            Some(AgentState::Running),
            Some(AgentState::Completed),
            true,
        ));

        // A repeated signal must not resurrect a marker the user cleared, even
        // while the pane is unattended.
        assert!(!unseen_after_status(
            false,
            Some(AgentState::Completed),
            Some(AgentState::Completed),
            false,
        ));
        assert!(unseen_after_status(
            true,
            Some(AgentState::Completed),
            Some(AgentState::Completed),
            false,
        ));

        // Starting a new turn or going idle no longer needs attention.
        assert!(!unseen_after_status(
            true,
            Some(AgentState::Completed),
            Some(AgentState::Running),
            false,
        ));
        assert!(!unseen_after_status(
            true,
            Some(AgentState::Waiting),
            None,
            false,
        ));
    }
}
