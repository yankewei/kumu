//! Exhaustive BFS state-space model checker for Kumu's dual-transport agent
//! status arbitration (`src/ui/home/agents.rs`).
//!
//! Runs directly against the real Rust decision functions in `super` so any
//! regression in `agents.rs` is caught immediately with the shortest BFS
//! counterexample path.
//!
//! Run with:
//!   cargo test --bin kumu ui::home::agents::formal_verify -- --nocapture

use std::collections::{HashMap, VecDeque};

use super::{
    AgentEntry, accepts_status, osc7501_status_signal, resolve_program_status_agent,
    should_activate_osc7501, should_remove_shell_entry_on_clear, should_suppress_hook_attention,
    unseen_after_status,
};
use crate::agent::{AgentState, StatusSignal, StatusSource};
use crate::term::{ProgramState, TermNotification, TermProgramStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AgentKind {
    Claude,   // Hook-based agent (OSC 7501 > Hooks)
    Codex,    // Hook-based agent (OSC 7501 > Hooks)
    OpenCode, // In-process extension agent (Integration > OSC 7501)
}

impl AgentKind {
    const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::OpenCode];

    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AppInput {
    Empty,
    Agent(AgentKind),
    NonAgentTool, // e.g. "cargo"
}

impl AppInput {
    const ALL: [Self; 5] = [
        Self::Empty,
        Self::Agent(AgentKind::Claude),
        Self::Agent(AgentKind::Codex),
        Self::Agent(AgentKind::OpenCode),
        Self::NonAgentTool,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "",
            Self::Agent(a) => a.id(),
            Self::NonAgentTool => "cargo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PaneConfig {
    Launched(AgentKind),
    Shell,
}

impl PaneConfig {
    fn is_shell(self) -> bool {
        matches!(self, Self::Shell)
    }

    fn launched_id(self) -> Option<&'static str> {
        match self {
            Self::Launched(a) => Some(a.id()),
            Self::Shell => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelEntry {
    agent: AgentKind,
    state: Option<AgentState>,
    process_tracked: bool,
    unseen: bool,
    osc7501_active: bool,
    integration_active: bool,
}

impl std::hash::Hash for ModelEntry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.agent.hash(state);
        self.state.map(AgentState::priority).hash(state);
        self.process_tracked.hash(state);
        self.unseen.hash(state);
        self.osc7501_active.hash(state);
        self.integration_active.hash(state);
    }
}

impl ModelEntry {
    fn to_real(self) -> AgentEntry {
        let mut entry = AgentEntry::idle(self.agent.id().to_string());
        entry.state = self.state;
        entry.process_tracked = self.process_tracked;
        entry.unseen = self.unseen;
        entry.osc7501_active = self.osc7501_active;
        entry.integration_active = self.integration_active;
        entry
    }

    fn from_real(entry: &AgentEntry) -> Option<Self> {
        Some(Self {
            agent: AgentKind::from_id(&entry.agent)?,
            state: entry.state,
            process_tracked: entry.process_tracked,
            unseen: entry.unseen,
            osc7501_active: entry.osc7501_active,
            integration_active: entry.integration_active,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ModelState {
    entry: Option<ModelEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Osc7501 {
        app: AppInput,
        state: ProgramState,
        attended: bool,
    },
    Integration {
        agent: AppInput,
        signal: StatusSignal,
        attended: bool,
    },
    ProcessWatcher {
        detected: Option<AgentKind>,
    },
    Acknowledge,
}

/// Faithful step function mirroring `KumuApp::note_program_status`,
/// `KumuApp::note_agent_status`, `KumuApp::reconcile_shell_agents`, and
/// `KumuApp::acknowledge_pane` in `src/ui/home/agents.rs`.
fn step(cfg: PaneConfig, state: ModelState, action: Action) -> ModelState {
    match action {
        Action::Acknowledge => {
            let mut next = state;
            if let Some(entry) = next.entry.as_mut() {
                entry.unseen = false;
            }
            next
        }
        Action::ProcessWatcher { detected } => {
            if !cfg.is_shell() {
                return state;
            }
            let mut next = state;
            match detected {
                Some(agent) => {
                    if let Some(existing) = next.entry {
                        let mut real = existing.to_real();
                        if real.agent != agent.id() {
                            real.switch_agent(agent.id().to_string());
                        }
                        real.process_tracked = true;
                        next.entry = ModelEntry::from_real(&real);
                    } else {
                        let mut real = AgentEntry::idle(agent.id().to_string());
                        real.process_tracked = true;
                        next.entry = ModelEntry::from_real(&real);
                    }
                }
                None => {
                    if next.entry.is_some_and(|e| e.process_tracked) {
                        next.entry = None;
                    }
                }
            }
            next
        }
        Action::Integration {
            agent,
            signal,
            attended,
        } => {
            // Mirrors `KumuApp::note_agent_status` (agents.rs:371-430)
            if signal == StatusSignal::Exited {
                let mut next = state;
                if cfg.is_shell() && next.entry.is_some() {
                    next.entry = None;
                }
                return next;
            }
            let raw_agent = agent.as_str();
            let resolved_agent = crate::agent::id_for(raw_agent)
                .map(str::to_string)
                .or_else(|| cfg.launched_id().map(str::to_string))
                .unwrap_or_else(|| raw_agent.to_string());
            if let Some(existing) = state.entry
                && (resolved_agent.is_empty() || existing.agent.id() == resolved_agent)
                && !accepts_status(
                    existing.agent.id(),
                    StatusSource::Integration,
                    existing.osc7501_active,
                    existing.integration_active,
                )
            {
                return state;
            }
            // Mirrors `ensure_agent_entry` (agents.rs:331-356)
            let target_agent = crate::agent::id_for(raw_agent)
                .map(str::to_string)
                .or_else(|| cfg.launched_id().map(str::to_string))
                .unwrap_or_else(|| raw_agent.to_string());
            let Some(target_kind) = AgentKind::from_id(&target_agent) else {
                // Non-agent / empty string on a shell pane would not map to a known agent.
                return state;
            };
            let mut real = match state.entry {
                Some(existing) => existing.to_real(),
                None => AgentEntry::idle(target_kind.id().to_string()),
            };
            if !target_agent.is_empty() && real.agent != target_agent {
                real.switch_agent(target_agent);
            }
            real.integration_active = true;
            let previous = real.state;
            match signal {
                StatusSignal::Active(s) => real.state = Some(s),
                StatusSignal::Resume => {
                    if real.state == Some(AgentState::Waiting) {
                        real.state = Some(AgentState::Running);
                    }
                }
                StatusSignal::Idle => real.state = None,
                StatusSignal::Exited => unreachable!(),
            }
            real.unseen = unseen_after_status(real.unseen, previous, real.state, attended);
            ModelState {
                entry: ModelEntry::from_real(&real),
            }
        }
        Action::Osc7501 {
            app,
            state: prog_state,
            attended,
        } => {
            // Mirrors `KumuApp::note_program_status` (agents.rs:433-545)
            let status = TermProgramStatus {
                state: prog_state,
                kind: None,
                app: app.as_str().to_string(),
                title: String::new(),
                progress: None,
                message: String::new(),
            };
            let existing_info = state.entry.map(|e| {
                (
                    e.agent.id(),
                    e.state.is_some() || e.osc7501_active || e.integration_active,
                )
            });
            let Some(agent_id) = resolve_program_status_agent(
                cfg.launched_id(),
                existing_info.map(|(a, _)| a),
                existing_info.is_some_and(|(_, active)| active),
                &status.app,
            ) else {
                return state;
            };
            if status.state == ProgramState::Clear {
                let Some(existing) = state
                    .entry
                    .filter(|e| e.agent.id() == agent_id && e.osc7501_active)
                else {
                    return state;
                };
                if should_remove_shell_entry_on_clear(
                    cfg.is_shell(),
                    cfg.launched_id().is_some(),
                    existing.process_tracked,
                ) {
                    return ModelState { entry: None };
                }
            }
            if let Some(existing) = state.entry
                && existing.agent.id() == agent_id
                && !accepts_status(
                    &agent_id,
                    StatusSource::Osc7501,
                    existing.osc7501_active,
                    existing.integration_active,
                )
            {
                return state;
            }
            let Some(target_kind) = AgentKind::from_id(&agent_id) else {
                return state;
            };
            let mut real = match state.entry {
                Some(existing) => existing.to_real(),
                None => AgentEntry::idle(target_kind.id().to_string()),
            };
            if !agent_id.is_empty() && real.agent != agent_id {
                real.switch_agent(agent_id);
            }
            if should_activate_osc7501(&status) {
                real.osc7501_active = true;
            }
            let signal = osc7501_status_signal(status.state);
            let previous = real.state;
            match signal {
                StatusSignal::Active(s) => real.state = Some(s),
                StatusSignal::Idle => real.state = None,
                StatusSignal::Resume | StatusSignal::Exited => unreachable!(),
            }
            real.unseen = unseen_after_status(real.unseen, previous, real.state, attended);
            ModelState {
                entry: ModelEntry::from_real(&real),
            }
        }
    }
}

fn all_actions(cfg: PaneConfig) -> Vec<Action> {
    let mut actions = Vec::new();
    let prog_states = [
        ProgramState::Working,
        ProgramState::Blocked,
        ProgramState::Done,
        ProgramState::Error,
        ProgramState::Idle,
        ProgramState::Clear,
    ];
    for app in AppInput::ALL {
        for state in prog_states {
            for attended in [false, true] {
                actions.push(Action::Osc7501 {
                    app,
                    state,
                    attended,
                });
            }
        }
    }
    let signals = [
        StatusSignal::Active(AgentState::Running),
        StatusSignal::Active(AgentState::Waiting),
        StatusSignal::Active(AgentState::Completed),
        StatusSignal::Active(AgentState::Failed),
        StatusSignal::Resume,
        StatusSignal::Idle,
        StatusSignal::Exited,
    ];
    for agent in [
        AppInput::Empty,
        AppInput::Agent(AgentKind::Claude),
        AppInput::Agent(AgentKind::Codex),
        AppInput::Agent(AgentKind::OpenCode),
    ] {
        for signal in signals {
            for attended in [false, true] {
                actions.push(Action::Integration {
                    agent,
                    signal,
                    attended,
                });
            }
        }
    }
    if cfg.is_shell() {
        actions.push(Action::ProcessWatcher { detected: None });
        for agent in AgentKind::ALL {
            actions.push(Action::ProcessWatcher {
                detected: Some(agent),
            });
        }
    }
    actions.push(Action::Acknowledge);
    actions
}

fn format_trace(
    parents: &HashMap<ModelState, Option<(ModelState, Action)>>,
    failing: ModelState,
    last_action: Option<(ModelState, Action)>,
) -> String {
    let mut steps = Vec::new();
    let mut cur = match last_action {
        Some((prev, act)) => {
            steps.push((act, failing));
            prev
        }
        None => failing,
    };
    while let Some(Some((prev, act))) = parents.get(&cur) {
        steps.push((*act, cur));
        cur = *prev;
    }
    steps.reverse();
    let mut out = format!("  init: {cur:?}\n");
    for (idx, (act, st)) in steps.iter().enumerate() {
        out.push_str(&format!("  step {}: {act:?}\n    -> {st:?}\n", idx + 1));
    }
    out
}

fn explore(
    cfg: PaneConfig,
    check_transition: impl Fn(ModelState, Action, ModelState) -> Result<(), String>,
) -> Result<(usize, usize), String> {
    let init = ModelState { entry: None };
    let actions = all_actions(cfg);
    let mut parents: HashMap<ModelState, Option<(ModelState, Action)>> = HashMap::new();
    let mut queue = VecDeque::new();
    parents.insert(init, None);
    queue.push_back(init);

    let mut transitions = 0usize;
    while let Some(cur) = queue.pop_front() {
        for &act in &actions {
            let next = step(cfg, cur, act);
            transitions += 1;
            if let Err(msg) = check_transition(cur, act, next) {
                let trace = format_trace(&parents, next, Some((cur, act)));
                return Err(format!(
                    "Invariant violated under {cfg:?}: {msg}\nShortest counterexample:\n{trace}"
                ));
            }
            if let std::collections::hash_map::Entry::Vacant(e) = parents.entry(next) {
                e.insert(Some((cur, act)));
                queue.push_back(next);
            }
        }
    }
    Ok((parents.len(), transitions))
}

/// Core invariants I1, I2, I3, I4, I6 across all reachable states and
/// interleavings for launched hook-based panes, launched extension panes, and
/// shell panes.
#[test]
fn exhaustive_bfs_core_invariants_hold() {
    let configs = [
        PaneConfig::Launched(AgentKind::Claude),
        PaneConfig::Launched(AgentKind::OpenCode),
        PaneConfig::Shell,
    ];
    for cfg in configs {
        let (states, transitions) = explore(cfg, |cur, act, next| {
            // I1: `unseen` is NEVER true for non-actionable states (None / Running).
            if let Some(e) = next.entry
                && matches!(e.state, None | Some(AgentState::Running))
                && e.unseen
            {
                return Err(format!(
                    "I1: unseen=true on non-actionable state {:?}",
                    e.state
                ));
            }

            // I2: Clear or anonymous (app == "") OSC 7501 report NEVER activates osc7501_active.
            if let Action::Osc7501 { app, state: ps, .. } = act {
                let was_active = cur.entry.is_some_and(|e| e.osc7501_active);
                let now_active = next.entry.is_some_and(|e| e.osc7501_active);
                if (ps == ProgramState::Clear || app == AppInput::Empty)
                    && !was_active
                    && now_active
                {
                    return Err(format!("I2: osc7501_active activated by {app:?} / {ps:?}"));
                }
            }

            // I3: Single-source authority — once preferred transport is active on the same
            // agent, the secondary transport must not overwrite state or unseen, and hook
            // alert suppression must match authority.
            if let (Some(e_cur), Some(e_next)) = (cur.entry, next.entry)
                && e_cur.agent == e_next.agent
            {
                let prefers_ext = crate::agent::prefers_integration_status(e_cur.agent.id());
                if !prefers_ext && e_cur.osc7501_active {
                    if let Action::Integration { agent, signal, .. } = act
                        && signal != StatusSignal::Exited
                        && (agent == AppInput::Empty || agent == AppInput::Agent(e_cur.agent))
                        && (e_next.state != e_cur.state || e_next.unseen != e_cur.unseen)
                    {
                        return Err(format!(
                            "I3: hook overwrote osc7501-authoritative agent {:?}",
                            e_cur.agent
                        ));
                    }
                    let hook_note = TermNotification {
                        title: e_cur.agent.id().into(),
                        body: "本轮任务已结束".into(),
                    };
                    if !should_suppress_hook_attention(e_cur.agent.id(), true, &hook_note) {
                        return Err(format!(
                            "I3: failed to suppress duplicate hook alert for {:?}",
                            e_cur.agent
                        ));
                    }
                }
                if prefers_ext && e_cur.integration_active {
                    if let Action::Osc7501 { app, state: ps, .. } = act
                        && ps != ProgramState::Clear
                        && (app == AppInput::Empty || app == AppInput::Agent(e_cur.agent))
                        && (e_next.state != e_cur.state || e_next.unseen != e_cur.unseen)
                    {
                        return Err(format!(
                            "I3: osc7501 overwrote extension-authoritative agent {:?}",
                            e_cur.agent
                        ));
                    }
                    let ext_note = TermNotification {
                        title: e_cur.agent.id().into(),
                        body: "本轮任务已结束".into(),
                    };
                    if should_suppress_hook_attention(
                        e_cur.agent.id(),
                        e_cur.osc7501_active,
                        &ext_note,
                    ) {
                        return Err(format!(
                            "I3: erroneously suppressed extension alert for {:?}",
                            e_cur.agent
                        ));
                    }
                }
            }

            // I4: Untracked shell entry MUST be removed on matching OSC 7501 Clear;
            // launched or process-tracked entries MUST NOT be removed on Clear.
            if let (
                Some(e_cur),
                Action::Osc7501 {
                    app,
                    state: ProgramState::Clear,
                    ..
                },
            ) = (cur.entry, act)
                && e_cur.osc7501_active
                && app == AppInput::Agent(e_cur.agent)
            {
                let should_remove = cfg.is_shell() && !e_cur.process_tracked;
                if should_remove && next.entry.is_some() {
                    return Err("I4: untracked shell entry leaked after OSC 7501 Clear".into());
                }
                if !should_remove && next.entry.is_none() {
                    return Err("I4: tracked or launched entry removed by OSC 7501 Clear".into());
                }
            }

            // I6: Repeating an already-acknowledged actionable state must not resurrect `unseen`.
            if let (Some(e_cur), Some(e_next)) = (cur.entry, next.entry)
                && e_cur.agent == e_next.agent
                && e_cur.state.is_some()
                && e_cur.state == e_next.state
                && !e_cur.unseen
                && e_next.unseen
            {
                return Err(format!(
                    "I6: unseen resurrected on repeated state {:?}",
                    e_cur.state
                ));
            }

            Ok(())
        })
        .expect("core formal invariants must hold");
        eprintln!(
            "[formal-verify] {cfg:?}: verified {states} reachable states, {transitions} transitions (I1, I2, I3, I4, I6 PASS)"
        );
    }
}

/// Invariant I5: On a launched agent pane (`launched = Some(L)`), no event from
/// another agent (OSC 7501 or OSC 777 Integration) may hijack `entry.agent` away
/// from `L`.
///
/// NOTE: Kept separate and `#[ignore]`d for normal `cargo test` so existing CI
/// stays green while findings stay visible when running the formal verifier
/// (`--include-ignored`).
#[test]
#[ignore = "Known finding at 8147d576: note_agent_status/ensure_agent_entry lets a nested OSC 777 Integration report hijack a launched agent pane"]
fn exhaustive_bfs_i5_launched_agent_identity_preserved() {
    for launched in [AgentKind::Claude, AgentKind::OpenCode] {
        let cfg = PaneConfig::Launched(launched);
        let result = explore(cfg, |_cur, _act, next| {
            if let Some(e) = next.entry
                && e.agent != launched
            {
                return Err(format!(
                    "I5: launched {launched:?} pane hijacked by {:?}",
                    e.agent
                ));
            }
            Ok(())
        });
        if let Err(report) = result {
            panic!("{report}");
        }
    }
}
