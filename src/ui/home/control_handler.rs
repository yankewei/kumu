use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui_kit::*;

use super::KumuApp;
use super::panes::{PaneDirection, PaneId, ProjectTabs};
use crate::agent::launch::PaneLaunch;
use crate::term::KeyInput;

/// How long a control split waits for the new agent to draw its prompt before
/// the text is sent anyway. An agent TUI that needs longer than this is still
/// started; only the prompt is reported as late.
const PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(12);
/// How long the new agent's first frame is left alone after it appears, so the
/// paste does not race the TUI finishing its startup.
const PROMPT_SETTLE: Duration = Duration::from_millis(400);

impl KumuApp {
    /// Serve the local control socket a program inside a pane uses to read its
    /// neighbours ("what is on the terminal to my right"). The accept loop
    /// forwards here so every answer comes from the thread that owns the
    /// workspace.
    pub(super) fn start_control(&mut self, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        cx.spawn(async move |this, cx| {
            while let Some(command) = rx.next().await {
                let _ = this.update(cx, |app, cx| app.handle_control_command(command, cx));
            }
        })
        .detach();
        self.control = crate::control::start(tx);
    }

    fn handle_control_command(&mut self, command: crate::control::Command, cx: &mut Context<Self>) {
        use crate::control::{Command, ControlError, PaneContent, SendResult};

        match command {
            Command::Panes { reply } => {
                let _ = reply.send(Ok(self.control_panes(cx)));
            }
            Command::Read {
                target,
                from,
                history,
                reply,
            } => {
                let pane = match self.control_resolve(&target, from, cx) {
                    Ok(pane) => pane,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let term = self.workspace.read(cx).terminal_for_pane(pane.0);
                let (Some(term), Some(summary)) = (term, self.control_summary(pane, cx)) else {
                    let _ = reply.send(Err(ControlError::new(
                        "paneGone",
                        format!("pane {} is not running", pane.0),
                    )));
                    return;
                };
                // A terminal whose process is gone can not read its screen: say
                // so instead of letting the read time out with no answer.
                if summary.exited {
                    let _ = reply.send(Err(ControlError::new(
                        "paneGone",
                        format!("pane {} is not running", pane.0),
                    )));
                    return;
                }
                // Reading scrollback pages the terminal's viewport, so it runs
                // there (and off both this thread and the UI).
                cx.spawn(async move |_, cx| {
                    let text = cx
                        .background_spawn(async move { term.content(history) })
                        .await;
                    let answer = match text {
                        Some(text) => Ok(PaneContent {
                            pane: summary,
                            text,
                        }),
                        None => Err(ControlError::new("timeout", "the terminal did not answer")),
                    };
                    let _ = reply.send(answer);
                })
                .detach();
            }
            Command::Split {
                direction,
                agent,
                from,
                prompt,
                cancelled,
                reply,
            } => self.control_split(direction, agent, from, prompt, cancelled, reply, cx),
            Command::Send {
                target,
                from,
                text,
                enter,
                reply,
            } => {
                let answer = self
                    .control_resolve(&target, from, cx)
                    .and_then(|pane| self.control_deliver(pane, &text, enter, cx));
                let _ = reply.send(answer.map(|pane| SendResult {
                    pane,
                    submitted: enter,
                }));
            }
        }
    }

    /// Start an agent in a new pane beside `from` (or the focused pane). The
    /// caller keeps keyboard focus: a split requested by an agent should not
    /// move the user's typing to the pane it opened.
    #[allow(clippy::too_many_arguments)]
    fn control_split(
        &mut self,
        direction: String,
        agent: String,
        from: Option<u64>,
        prompt: Option<String>,
        cancelled: Arc<AtomicBool>,
        reply: std::sync::mpsc::Sender<
            Result<crate::control::SplitResult, crate::control::ControlError>,
        >,
        cx: &mut Context<Self>,
    ) {
        use crate::control::{ControlError, SplitResult};

        const MAX_CONTROL_PANES_PER_TAB: usize = 16;

        // An empty prompt is no prompt: the split must not wait on a paste
        // that has nothing to type.
        let prompt = prompt.filter(|prompt| !prompt.trim().is_empty());
        let Some(agent) = crate::agent::id_for(&agent) else {
            let _ = reply.send(Err(ControlError::new(
                "unknownAgent",
                format!("unknown agent '{agent}'"),
            )));
            return;
        };
        if !crate::agent::installed(agent) {
            let _ = reply.send(Err(ControlError::new(
                "agentNotInstalled",
                format!("{agent} is not installed on this Mac"),
            )));
            return;
        }
        let axis = match direction.trim().to_ascii_lowercase().as_str() {
            "right" | "horizontal" => Axis::Horizontal,
            "down" | "vertical" => Axis::Vertical,
            other => {
                let _ = reply.send(Err(ControlError::new(
                    "unknownDirection",
                    format!("unknown direction '{other}': Kumu splits right or down"),
                )));
                return;
            }
        };
        let parent = self.control_anchor(from);
        let Some((project, tab_index)) = self.pane_tab(parent) else {
            let _ = reply.send(Err(ControlError::new(
                "unknownPane",
                format!("pane {} is not open", parent.0),
            )));
            return;
        };
        if self.workspace.read(cx).project(project).is_none() {
            let _ = reply.send(Err(ControlError::new(
                "unknownPane",
                format!("pane {} is not open", parent.0),
            )));
            return;
        }
        let tab_panes = self
            .tabs_by_project
            .get(&project)
            .and_then(|tabs| tabs.tabs.get(tab_index))
            .map_or(0, |tab| tab.layout.panes().len());
        if tab_panes >= MAX_CONTROL_PANES_PER_TAB {
            let _ = reply.send(Err(ControlError::new(
                "tooManyPanes",
                format!("tab already has {tab_panes} panes"),
            )));
            return;
        }
        let launch = PaneLaunch::Agent {
            agent: agent.to_owned(),
            session: None,
        };
        let is_visible_tab = self.selected == Some(project)
            && self
                .tabs_by_project
                .get(&project)
                .is_some_and(|tabs| tabs.selected == tab_index);
        let added = if is_visible_tab {
            // Keep keyboard focus where the user left it without switching
            // projects or tearing down open find bars.
            let previous_focus = self.focused;
            let before = self.focused;
            self.split_pane_with(parent, axis, launch, cx);
            let added = self.focused;
            if added == before {
                self.restore_control_focus(previous_focus, parent, cx);
                let _ = reply.send(Err(ControlError::new(
                    "splitFailed",
                    "this pane could not be split",
                )));
                return;
            }
            self.restore_control_focus(previous_focus, parent, cx);
            added
        } else {
            // Split the background tab directly without changing `self.selected`,
            // so the user's active project, open find bar, and plugins are untouched.
            let Some(added) =
                self.split_background_pane(project, tab_index, parent, axis, launch, cx)
            else {
                let _ = reply.send(Err(ControlError::new(
                    "splitFailed",
                    "this pane could not be split",
                )));
                return;
            };
            added
        };

        let Some(prompt) = prompt else {
            let answer = self.control_summary(added, cx).ok_or_else(|| {
                ControlError::new("paneGone", format!("pane {} did not start", added.0))
            });
            let _ = reply.send(answer.map(|pane| SplitResult { pane }));
            return;
        };
        // The agent's TUI has to draw its input line before the text means
        // anything to it, so wait for the first frame and let it settle.
        cx.spawn(async move |this, cx| {
            let deadline = Instant::now() + PROMPT_READY_TIMEOUT;
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let state = this.update(cx, |app, cx| app.control_prompt_state(added, cx));
                match state {
                    Ok(Ok(true)) => break,
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                    Err(_) => {
                        let _ = reply
                            .send(Err(ControlError::new("unavailable", "kumu is not running")));
                        return;
                    }
                }
                if Instant::now() >= deadline {
                    let _ = reply.send(Err(ControlError::new(
                        "promptTimeout",
                        format!(
                            "pane {} started but its agent did not draw in {}s",
                            added.0,
                            PROMPT_READY_TIMEOUT.as_secs()
                        ),
                    )));
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
            }
            cx.background_executor().timer(PROMPT_SETTLE).await;
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let sent = this.update(cx, |app, cx| app.control_deliver(added, &prompt, true, cx));
            let answer = match sent {
                Ok(answer) => answer.map(|pane| SplitResult { pane }),
                Err(_) => Err(ControlError::new("unavailable", "kumu is not running")),
            };
            let _ = reply.send(answer);
        })
        .detach();
    }

    /// Split a pane inside a non-visible project/tab without disrupting the
    /// user's active view or find bar.
    fn split_background_pane(
        &mut self,
        project_id: i64,
        tab_index: usize,
        parent: PaneId,
        axis: Axis,
        launch: PaneLaunch,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        let new_id = PaneId(self.next_pane);
        let group_id = self.next_group;
        let tab = self
            .tabs_by_project
            .get_mut(&project_id)
            .and_then(|tabs| tabs.tabs.get_mut(tab_index))?;
        if !tab.layout.split(parent, axis, new_id, group_id) {
            return None;
        }
        let made_group = tab.layout.contains_group(group_id);
        self.next_pane += 1;
        self.next_group += 1;
        if made_group {
            let state = Self::new_group_state(&[], cx);
            self.group_states.insert(group_id, state);
        }
        if launch.is_shell() {
            self.launches.remove(&new_id);
        } else {
            self.launches.insert(new_id, launch.clone());
        }
        self.install_pane(new_id, cx);
        let started = self
            .workspace
            .update(cx, |ws, cx| ws.prestart(new_id.0, project_id, &launch, cx));
        if !started {
            return None;
        }
        self.persist_workspace(cx);
        Some(new_id)
    }

    /// Put focus back where the user left it after a control request moved it.
    /// The previous pane's own project comes back with it, so a split asked
    /// for by a background project leaves the window where the user was. When
    /// that pane is gone, the caller's pane keeps the focus instead.
    fn restore_control_focus(
        &mut self,
        previous: PaneId,
        fallback: PaneId,
        cx: &mut Context<Self>,
    ) {
        let pane =
            if self.panes.contains_key(&previous) && self.pane_project(previous, cx).is_some() {
                previous
            } else {
                fallback
            };
        self.pending_focus = Some(pane);
        self.focus_pane(pane, cx);
    }

    /// Whether a freshly started pane has drawn its own input line, or why it
    /// never will: a pane that failed to start or stopped being an agent is
    /// reported at once instead of after the ready timeout.
    fn control_prompt_state(
        &self,
        pane: PaneId,
        cx: &Context<Self>,
    ) -> Result<bool, crate::control::ControlError> {
        use crate::control::ControlError;

        let workspace = self.workspace.read(cx);
        if !matches!(self.launches.get(&pane), Some(PaneLaunch::Agent { .. }))
            || workspace.pane_is_shell(pane.0) != Some(false)
        {
            return Err(ControlError::new(
                "notAgent",
                format!("pane {} is no longer an agent pane", pane.0),
            ));
        }
        let Some(term) = workspace.terminal_for_pane(pane.0) else {
            return Err(ControlError::new(
                "paneGone",
                format!("pane {} is not running", pane.0),
            ));
        };
        let snapshot = term.snapshot();
        if let Some(error) = &snapshot.spawn_error {
            return Err(ControlError::new(
                "paneGone",
                format!("pane {} did not start: {error}", pane.0),
            ));
        }
        if snapshot.exited {
            return Err(ControlError::new(
                "paneGone",
                format!("pane {} exited before it could draw", pane.0),
            ));
        }
        Ok(snapshot
            .cells
            .iter()
            .any(|row| row.iter().any(|cell| !cell.text.trim().is_empty())))
    }

    /// Type `text` into an agent pane, optionally pressing Enter after it. A
    /// shell pane is refused: text there would run as a command.
    fn control_deliver(
        &self,
        pane: PaneId,
        text: &str,
        enter: bool,
        cx: &mut Context<Self>,
    ) -> Result<crate::control::PaneSummary, crate::control::ControlError> {
        use crate::control::ControlError;

        if text.is_empty() {
            return Err(ControlError::new("emptyText", "there is no text to type"));
        }
        let workspace = self.workspace.read(cx);
        // Require the live PTY itself to have been started directly as an agent
        // (not a shell pane that adopted a session), so `pane send` can never
        // execute commands in an interactive shell.
        if !matches!(self.launches.get(&pane), Some(PaneLaunch::Agent { .. }))
            || workspace.pane_is_shell(pane.0) != Some(false)
        {
            return Err(ControlError::new(
                "notAgent",
                format!("pane {} is not an agent pane", pane.0),
            ));
        }
        let Some(term) = workspace.terminal_for_pane(pane.0) else {
            return Err(ControlError::new(
                "paneGone",
                format!("pane {} is not running", pane.0),
            ));
        };
        // A spawn failure keeps the pane (the header shows why) but leaves the
        // terminal thread gone: pasting there reports success and reaches
        // nobody.
        let snapshot = term.snapshot();
        if let Some(error) = &snapshot.spawn_error {
            return Err(ControlError::new(
                "paneGone",
                format!("pane {} did not start: {error}", pane.0),
            ));
        }
        if snapshot.exited {
            return Err(ControlError::new(
                "paneGone",
                format!("pane {} has exited", pane.0),
            ));
        }
        term.paste(text);
        if enter {
            // A bracketed paste can keep the TUI busy long enough to swallow an
            // Enter that arrives in the same read, so submit after it settles.
            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                term.key(KeyInput {
                    key: "enter".into(),
                    key_char: Some("\r".into()),
                    shift: false,
                    control: false,
                    alt: false,
                });
            })
            .detach();
        }
        self.control_summary(pane, cx)
            .ok_or_else(|| ControlError::new("paneGone", format!("pane {} is not running", pane.0)))
    }

    /// Every pane of every tab of every project, with the facts a reader needs
    /// to pick one.
    fn control_panes(&self, cx: &Context<Self>) -> Vec<crate::control::PaneSummary> {
        let mut panes: Vec<_> = self
            .tabs_by_project
            .values()
            .flat_map(ProjectTabs::panes)
            .filter_map(|pane| self.control_summary(pane, cx))
            .collect();
        panes.sort_by_key(|pane| pane.id);
        panes
    }

    /// One pane's facts. `None` when the pane has no live process to read.
    fn control_summary(
        &self,
        pane: PaneId,
        cx: &Context<Self>,
    ) -> Option<crate::control::PaneSummary> {
        use crate::control::PaneSummary;

        let project = self.pane_project(pane, cx)?;
        let workspace = self.workspace.read(cx);
        let term = workspace.terminal_for_pane(pane.0)?;
        let snapshot = term.snapshot();
        Some(PaneSummary {
            id: pane.0,
            project,
            project_name: workspace.project(project)?.name.clone(),
            title: snapshot.label(),
            command: snapshot.command.clone(),
            cwd: snapshot.cwd.clone(),
            agent: match self.launches.get(&pane) {
                Some(PaneLaunch::Agent { agent, .. })
                    if workspace.pane_is_shell(pane.0) == Some(false) =>
                {
                    Some(agent.clone())
                }
                _ => None,
            },
            focused: self.focused == pane,
            selected: self.selected == Some(project),
            cols: snapshot
                .cells
                .first()
                .map(|row| row.len().clamp(1, u16::MAX as usize) as u16)
                .unwrap_or(80),
            rows: snapshot.rows.max(1),
            exited: snapshot.exited,
        })
    }

    /// Turn a pane id, a direction, or `focused` into a pane that exists.
    /// A direction is relative to the caller's pane when it identified itself
    /// (via `KUMU_PANE`), else to the focused one.
    fn control_resolve(
        &self,
        target: &crate::control::PaneTarget,
        from: Option<u64>,
        _cx: &Context<Self>,
    ) -> Result<PaneId, crate::control::ControlError> {
        use crate::control::{ControlError, PaneTarget};

        let name = match target {
            PaneTarget::Id(id) => {
                let pane = PaneId(*id);
                return self.control_owns(pane).then_some(pane).ok_or_else(|| {
                    ControlError::new("unknownPane", format!("pane {id} is not open"))
                });
            }
            PaneTarget::Name(name) => name.trim().to_ascii_lowercase(),
        };
        let anchor = self.control_anchor(from);
        match name.as_str() {
            "" | "focused" => Ok(self.focused),
            "current" | "self" | "me" => Ok(anchor),
            "left" | "right" | "up" | "down" => {
                let direction = match name.as_str() {
                    "left" => PaneDirection::Left,
                    "right" => PaneDirection::Right,
                    "up" => PaneDirection::Up,
                    _ => PaneDirection::Down,
                };
                let Some((project, tab_index)) = self.pane_tab(anchor) else {
                    return Err(ControlError::new(
                        "unknownPane",
                        format!("pane {} is not open", anchor.0),
                    ));
                };
                self.tabs_by_project
                    .get(&project)
                    .and_then(|tabs| tabs.tabs.get(tab_index))
                    .and_then(|tab| tab.layout.neighbor_in(anchor, direction))
                    .ok_or_else(|| {
                        ControlError::new(
                            "noPane",
                            format!("pane {} has nothing on its {name}", anchor.0),
                        )
                    })
            }
            other => Err(ControlError::new(
                "unknownTarget",
                format!("unknown target '{other}': use a pane id, left/right/up/down, or focused"),
            )),
        }
    }

    /// The pane a relative request starts from: the caller's own pane when it
    /// said which one that is, else the focused pane.
    fn control_anchor(&self, from: Option<u64>) -> PaneId {
        from.filter(|id| self.control_owns(PaneId(*id)))
            .map(PaneId)
            .unwrap_or(self.focused)
    }

    fn control_owns(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
            || self
                .tabs_by_project
                .values()
                .any(|tabs| tabs.panes().contains(&pane))
    }
}
