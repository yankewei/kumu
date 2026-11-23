//! Live agent status tracking, title resolution, and background shell-agent watcher.

use crate::agent::launch::PaneLaunch;
use crate::agent::status::clean_osc_title;
use crate::agent::{AgentState, StatusSignal};
use crate::app::Workspace;
use crate::app::layout::PaneId;
use gpui::*;

use super::KumuApp;
use super::terminal::TerminalEvent;

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
        }
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
            TerminalEvent::Activated => self.focus_pane(pane, cx),
            TerminalEvent::ChromeChanged => cx.notify(),
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
            entry.agent = agent;
            entry.session_title = None;
            entry.prompt_title = None;
        }
        entry
    }

    /// Record a status signal an agent reported on its silent OSC channel.
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
                    .update(cx, |workspace, cx| workspace.unbind_pane_todos(pane.0, cx));
                self.sync_pane_agent_status(pane, cx);
                cx.notify();
            }
            return;
        }
        let entry = self.ensure_agent_entry(pane, agent, cx);
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
        match signal {
            StatusSignal::Active(AgentState::Completed) => {
                self.workspace.update(cx, |workspace, cx| {
                    workspace.mark_pane_todos_review(pane.0, cx)
                });
            }
            StatusSignal::Active(AgentState::Running) | StatusSignal::Resume => {
                self.workspace.update(cx, |workspace, cx| {
                    workspace.clear_pane_todos_review(pane.0, cx);
                });
            }
            _ => {}
        }
        self.sync_pane_agent_status(pane, cx);
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

    /// Push the active state the pane's header should draw (`None` for idle
    /// agents and plain shells).
    pub(super) fn sync_pane_agent_status(&self, pane: PaneId, cx: &mut Context<Self>) {
        let state = self.agent_status.get(&pane).and_then(|entry| entry.state);
        if let Some(view) = self.panes.get(&pane) {
            view.update(cx, |view, cx| view.set_agent_status(state, cx));
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
        for (pane_raw, detected_agent) in detected {
            let pane = PaneId(pane_raw);
            if self.workspace.read(cx).pane_is_shell(pane_raw) != Some(true) {
                continue;
            }
            match detected_agent {
                Some(agent) => {
                    if let Some(entry) = self.agent_status.get_mut(&pane) {
                        entry.process_tracked = true;
                    } else {
                        let osc_title = self
                            .workspace
                            .read(cx)
                            .pane_title(pane_raw)
                            .and_then(|title| clean_osc_title(&title, &agent));
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
        if !removed_panes.is_empty() {
            self.workspace.update(cx, |workspace, cx| {
                for pane in &removed_panes {
                    workspace.unbind_pane_todos(pane.0, cx);
                }
            });
            for pane in removed_panes {
                self.sync_pane_agent_status(pane, cx);
            }
        }
        if changed {
            cx.notify();
        }
    }
}
