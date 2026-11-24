use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::*;

use super::KumuApp;
use crate::agent::launch::PaneLaunch;
use crate::app::layout::{PaneDirection, PaneId, ProjectTabs, SplitAxis};
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
        use crate::control::{
            Command, ControlError, MAX_SCROLL_LINES, PaneContent, ScrollDirection, ScrollResult,
            SendResult,
        };

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
            Command::Scroll {
                target,
                from,
                direction,
                lines,
                col,
                row,
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
                if summary.exited {
                    let _ = reply.send(Err(ControlError::new(
                        "paneGone",
                        format!("pane {} is not running", pane.0),
                    )));
                    return;
                }
                let effective_lines = match lines {
                    Some(n) if (1..=MAX_SCROLL_LINES).contains(&n) => n,
                    Some(_) => {
                        let _ = reply.send(Err(ControlError::new(
                            "badRequest",
                            format!("lines must be between 1 and {MAX_SCROLL_LINES}"),
                        )));
                        return;
                    }
                    None => (summary.rows / 2).max(1),
                };
                let signed = match direction {
                    ScrollDirection::Up => -(effective_lines as isize),
                    ScrollDirection::Down => effective_lines as isize,
                };
                let before_rev = term.frame_rev();
                cx.spawn(async move |_, cx| {
                    let term_for_scroll = term.clone();
                    let outcome = cx
                        .background_spawn(async move {
                            term_for_scroll.scroll_control(signed, col, row)
                        })
                        .await;
                    let Some(outcome) = outcome else {
                        let _ = reply.send(Err(ControlError::new(
                            "timeout",
                            "the terminal did not answer",
                        )));
                        return;
                    };
                    if outcome.wait_redraw {
                        // Wait briefly for the TUI to process the wheel/arrow
                        // bytes and settle its redraw (bounded at 200ms).
                        let deadline = Instant::now() + Duration::from_millis(200);
                        let quiet = Duration::from_millis(40);
                        let mut last_rev = before_rev;
                        let mut changed_at: Option<Instant> = None;
                        while Instant::now() < deadline {
                            let rev = term.frame_rev();
                            if rev != last_rev {
                                last_rev = rev;
                                changed_at = Some(Instant::now());
                            } else if let Some(at) = changed_at
                                && at.elapsed() >= quiet
                            {
                                break;
                            }
                            cx.background_executor()
                                .timer(Duration::from_millis(10))
                                .await;
                        }
                    }
                    let _ = reply.send(Ok(ScrollResult {
                        pane: summary,
                        direction,
                        lines: effective_lines,
                        moved: outcome.moved,
                    }));
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
            Command::Close {
                target,
                from,
                allow_self,
                reply,
            } => {
                let answer = self.control_resolve(&target, from, cx).and_then(|pane| {
                    if !allow_self && from == Some(pane.0) {
                        return Err(ControlError::new(
                            "refuseSelf",
                            format!(
                                "refusing to close caller pane {}; pass --self to confirm",
                                pane.0
                            ),
                        ));
                    }
                    self.close_pane(pane, cx);
                    Ok(crate::control::CloseResult {
                        pane: pane.0,
                        closed: true,
                    })
                });
                let _ = reply.send(answer);
            }
            Command::Wait {
                target,
                from,
                timeout_secs,
                next,
                cancelled,
                reply,
            } => self.control_wait(target, from, timeout_secs, next, cancelled, reply, cx),
            Command::TodoList {
                from,
                status,
                mine,
                reply,
            } => {
                let answer = self.control_todo_board(from, cx).map(|board_id| {
                    let mut items = self.workspace.read(cx).todos_for_project(board_id);
                    if let Some(status) = status {
                        items.retain(|item| item.status == status);
                    }
                    if mine {
                        let caller = from.or(Some(self.focused.0));
                        items.retain(|item| item.pane_id == caller);
                    }
                    items
                });
                let _ = reply.send(answer);
            }
            Command::TodoShow { id, from, reply } => {
                let answer = self.control_todo_board(from, cx).and_then(|board_id| {
                    self.workspace
                        .read(cx)
                        .todo(id)
                        .filter(|item| item.project_id == board_id)
                        .ok_or_else(|| {
                            ControlError::new("unknownTodo", format!("todo #{id} not found"))
                        })
                });
                let _ = reply.send(answer);
            }
            Command::TodoAdd {
                from,
                title,
                description,
                status,
                bind,
                reply,
            } => {
                let answer = self.control_todo_board(from, cx).and_then(|board_id| {
                    let trimmed = title.trim();
                    if trimmed.is_empty() {
                        return Err(ControlError::new(
                            "badRequest",
                            "todo title cannot be empty",
                        ));
                    }
                    let (pane_id, agent_id, session_id) = if bind {
                        self.control_todo_binding(from)
                    } else {
                        (None, None, None)
                    };
                    self.workspace
                        .update(cx, |workspace, cx| {
                            workspace.add_todo(
                                board_id,
                                trimmed,
                                description.trim(),
                                status,
                                pane_id,
                                agent_id.as_deref(),
                                session_id.as_deref(),
                                cx,
                            )
                        })
                        .ok_or_else(|| ControlError::new("internal", "failed to create todo"))
                });
                let _ = reply.send(answer);
            }
            Command::TodoUpdate {
                id,
                from,
                title,
                description,
                status,
                awaiting_review,
                bind,
                unbind,
                reply,
            } => {
                let answer = self.control_todo_board(from, cx).and_then(|board_id| {
                    let existing = self
                        .workspace
                        .read(cx)
                        .todo(id)
                        .filter(|item| item.project_id == board_id)
                        .ok_or_else(|| {
                            ControlError::new("unknownTodo", format!("todo #{id} not found"))
                        })?;
                    let mut patch = crate::db::TodoPatch {
                        title: title.map(|t| t.trim().to_string()),
                        description: description.map(|d| d.trim().to_string()),
                        status,
                        awaiting_review,
                        ..Default::default()
                    };
                    if patch.title.as_deref().is_some_and(str::is_empty) {
                        return Err(ControlError::new(
                            "badRequest",
                            "todo title cannot be empty",
                        ));
                    }
                    if unbind {
                        patch.pane_id = Some(None);
                    } else if bind
                        || (awaiting_review == Some(true)
                            && existing.pane_id.is_none()
                            && from.is_some())
                    {
                        let (pane_id, agent_id, session_id) = self.control_todo_binding(from);
                        if pane_id.is_some() {
                            patch.pane_id = Some(pane_id);
                            if agent_id.is_some() {
                                patch.agent_id = Some(agent_id);
                            }
                            if session_id.is_some() {
                                patch.agent_session_id = Some(session_id);
                            }
                        }
                    }
                    self.workspace
                        .update(cx, |workspace, cx| workspace.update_todo(id, &patch, cx))
                        .ok_or_else(|| ControlError::new("internal", "failed to update todo"))
                });
                let _ = reply.send(answer);
            }
            Command::TodoRemove { id, from, reply } => {
                let answer = self.control_todo_board(from, cx).and_then(|board_id| {
                    let exists = self
                        .workspace
                        .read(cx)
                        .todo(id)
                        .is_some_and(|item| item.project_id == board_id);
                    if !exists {
                        return Err(ControlError::new(
                            "unknownTodo",
                            format!("todo #{id} not found"),
                        ));
                    }
                    Ok(self
                        .workspace
                        .update(cx, |workspace, cx| workspace.delete_todo(id, cx)))
                });
                let _ = reply.send(answer);
            }
            Command::TodoImport {
                from,
                input,
                status,
                bind,
                reply,
            } => {
                let board_id = match self.control_todo_board(from, cx) {
                    Ok(id) => id,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let Some(repo_dir) = self
                    .workspace
                    .read(cx)
                    .project(board_id)
                    .map(|p| std::path::PathBuf::from(&p.path))
                else {
                    let _ = reply.send(Err(ControlError::new(
                        "noProject",
                        "active project not found",
                    )));
                    return;
                };
                let (pane_id, agent_id, session_id) = if bind {
                    self.control_todo_binding(from)
                } else {
                    (None, None, None)
                };
                cx.spawn(async move |this, cx| {
                    let fetched = cx
                        .background_executor()
                        .spawn(async move { crate::tracker::fetch_task(&input, &repo_dir) })
                        .await;
                    let answer = match fetched {
                        Ok(task) => this
                            .update(cx, |app, cx| {
                                app.workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.upsert_imported_todo(
                                            board_id,
                                            &task,
                                            status,
                                            pane_id,
                                            agent_id.as_deref(),
                                            session_id.as_deref(),
                                            cx,
                                        )
                                    })
                                    .map(|(todo, created)| crate::control::TodoImportResult {
                                        todo,
                                        created,
                                    })
                                    .ok_or_else(|| {
                                        ControlError::new(
                                            "internal",
                                            "failed to save imported todo",
                                        )
                                    })
                            })
                            .unwrap_or_else(|_| {
                                Err(ControlError::new("unavailable", "kumu window closed"))
                            }),
                        Err(error) => Err(ControlError::new("importFailed", error.to_string())),
                    };
                    let _ = reply.send(answer);
                })
                .detach();
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
            "right" | "horizontal" => SplitAxis::Horizontal,
            "down" | "vertical" => SplitAxis::Vertical,
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

    /// Block a `kumu pane wait` caller until the target agent pane reaches a
    /// terminal/attention state (`waiting`, `completed`, `failed`) or exits.
    #[allow(clippy::too_many_arguments)]
    fn control_wait(
        &mut self,
        target: crate::control::PaneTarget,
        from: Option<u64>,
        timeout_secs: u64,
        next: bool,
        cancelled: Arc<AtomicBool>,
        reply: std::sync::mpsc::Sender<
            Result<crate::control::WaitResult, crate::control::ControlError>,
        >,
        cx: &mut Context<Self>,
    ) {
        use crate::agent::AgentState;
        use crate::control::{ControlError, WaitResult};

        let pane = match self.control_resolve(&target, from, cx) {
            Ok(pane) => pane,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let is_agent = self.agent_status.contains_key(&pane)
            || matches!(self.launches.get(&pane), Some(PaneLaunch::Agent { .. }));
        if !is_agent {
            let _ = reply.send(Err(ControlError::new(
                "notAgent",
                format!("pane {} is not an agent pane", pane.0),
            )));
            return;
        }

        let is_settled = |state: Option<AgentState>| {
            matches!(
                state,
                Some(AgentState::Waiting | AgentState::Completed | AgentState::Failed)
            )
        };
        let state_label = |state: Option<AgentState>| -> String {
            match state {
                Some(AgentState::Running) => "running".into(),
                Some(AgentState::Waiting) => "waiting".into(),
                Some(AgentState::Completed) => "completed".into(),
                Some(AgentState::Failed) => "failed".into(),
                None => "idle".into(),
            }
        };

        let initial_state = self.agent_status.get(&pane).and_then(|e| e.state);
        let initial_summary = self.control_summary(pane, cx);
        if !next
            && is_settled(initial_state)
            && let Some(summary) = initial_summary
        {
            let _ = reply.send(Ok(WaitResult {
                pane: summary,
                state: state_label(initial_state),
                elapsed_ms: 0,
            }));
            return;
        }

        let started = Instant::now();
        let deadline = started + Duration::from_secs(timeout_secs);
        let mut saw_transition = !next || !is_settled(initial_state);
        let mut last_summary = initial_summary;

        cx.spawn(async move |this, cx| {
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                if cancelled.load(Ordering::Acquire) {
                    return;
                }

                let poll = this.update(cx, |app, cx| {
                    let summary = app.control_summary(pane, cx);
                    let agent_present = app.agent_status.contains_key(&pane)
                        || matches!(app.launches.get(&pane), Some(PaneLaunch::Agent { .. }));
                    let state = app.agent_status.get(&pane).and_then(|e| e.state);
                    (summary, agent_present, state)
                });

                match poll {
                    Ok((Some(summary), agent_present, state)) => {
                        let exited = summary.exited || !agent_present;
                        last_summary = Some(summary.clone());
                        if exited {
                            let _ = reply.send(Ok(WaitResult {
                                pane: summary,
                                state: "exited".into(),
                                elapsed_ms: started.elapsed().as_millis() as u64,
                            }));
                            return;
                        }
                        if !saw_transition && !is_settled(state) {
                            saw_transition = true;
                        }
                        if saw_transition && is_settled(state) {
                            let _ = reply.send(Ok(WaitResult {
                                pane: summary,
                                state: state_label(state),
                                elapsed_ms: started.elapsed().as_millis() as u64,
                            }));
                            return;
                        }
                    }
                    Ok((None, _, _)) => {
                        if let Some(summary) = last_summary {
                            let _ = reply.send(Ok(WaitResult {
                                pane: summary,
                                state: "exited".into(),
                                elapsed_ms: started.elapsed().as_millis() as u64,
                            }));
                        } else {
                            let _ = reply.send(Err(ControlError::new(
                                "paneGone",
                                format!("pane {} closed while waiting", pane.0),
                            )));
                        }
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
                        "waitTimeout",
                        format!("pane {} did not finish within {}s", pane.0, timeout_secs),
                    )));
                    return;
                }
            }
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
        axis: SplitAxis,
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
            self.group_ratios.remove(&group_id);
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

    /// Resolve the root repository board id for a `kumu todo` caller: prefers
    /// the caller's own `$KUMU_PANE`, else falls back to the selected project.
    fn control_todo_board(
        &self,
        from: Option<u64>,
        cx: &Context<Self>,
    ) -> Result<i64, crate::control::ControlError> {
        let project_id = from
            .and_then(|pane| self.pane_project(PaneId(pane), cx))
            .or(self.selected)
            .ok_or_else(|| {
                crate::control::ControlError::new("noProject", "no active project selected")
            })?;
        Ok(self.workspace.read(cx).board_project_id(project_id))
    }

    /// Capture `(pane_id, agent_id, session_id)` for a `kumu todo ... --bind` call.
    fn control_todo_binding(
        &self,
        from: Option<u64>,
    ) -> (Option<u64>, Option<String>, Option<String>) {
        let pane = self.control_anchor(from);
        if !self.control_owns(pane) {
            return (None, None, None);
        }
        let (agent_id, session_id) = match self.launches.get(&pane) {
            Some(PaneLaunch::Agent { agent, session }) => (Some(agent.clone()), session.clone()),
            _ => (
                self.agent_status
                    .get(&pane)
                    .map(|entry| entry.agent.clone()),
                None,
            ),
        };
        (Some(pane.0), agent_id, session_id)
    }

    /// Whether a freshly started pane has drawn its own input line, or why it
    /// never will: a pane that failed to start or stopped being an agent is
    /// reported at once instead of after the ready timeout.
    pub(super) fn control_prompt_state(
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
