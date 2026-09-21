//! What each agent offers — its session options and slash commands — as
//! discovered by probing it, and the bridge that carries agent events onto the
//! GPUI thread.

use std::collections::{HashMap, HashSet};

use futures::channel::mpsc::UnboundedReceiver;

use crate::agent;

/// An agent's session options: what it lets the user pick, each with the value
/// the agent reports as active (used as the default label before the user picks
/// one).
///
/// Which lists are empty depends on the agent — ACP agents have modes and a
/// reasoning level, Codex has a sandbox policy — so an empty list means "no such
/// axis here", and the matching picker renders nothing.
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub models: Vec<agent::ModelInfo>,
    pub current_model: Option<String>,
    pub modes: Vec<agent::ModeInfo>,
    pub current_mode: Option<String>,
    pub access: Vec<agent::AccessInfo>,
    pub current_access: Option<String>,
    pub sandboxes: Vec<agent::SandboxInfo>,
    pub current_sandbox: Option<String>,
    pub reasoning: Vec<agent::ReasoningInfo>,
    pub current_reasoning: Option<String>,
}

impl SessionOptions {
    /// The options an [`agent::Event::SessionOptions`] reports, with the agent
    /// that reported them; `None` for any other event.
    pub fn from_event(ev: &agent::Event) -> Option<(&'static str, Self)> {
        let agent::Event::SessionOptions {
            agent,
            models,
            current_model,
            modes,
            current_mode,
            access,
            current_access,
            sandboxes,
            current_sandbox,
            reasoning,
            current_reasoning,
        } = ev
        else {
            return None;
        };
        Some((
            *agent,
            Self {
                models: models.clone(),
                current_model: current_model.clone(),
                modes: modes.clone(),
                current_mode: current_mode.clone(),
                access: access.clone(),
                current_access: current_access.clone(),
                sandboxes: sandboxes.clone(),
                current_sandbox: current_sandbox.clone(),
                reasoning: reasoning.clone(),
                current_reasoning: current_reasoning.clone(),
            },
        ))
    }

    /// Whether the report names any axis at all. The axes are independent — an
    /// agent may offer any subset of them — so an entirely empty report is not
    /// worth caching.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
            && self.modes.is_empty()
            && self.sandboxes.is_empty()
            && self.reasoning.is_empty()
    }
}

/// What each agent offers, as its probe reported it, and which probes are
/// still out.
///
/// This is the agent in general — what a session of it starts from before its
/// own connection reports. A live session's report is its own and never lands
/// here (see [`super::Workspace::session_options`]), so one session's model or
/// effort cannot become every other session's default.
#[derive(Default)]
pub struct Discovery {
    catalogs: HashMap<&'static str, SessionOptions>,
    /// Slash commands per agent, cached apart from the options: those are axes
    /// with a value in force, re-reported whole, while a command list arrives on
    /// its own and is replaced only by another one.
    commands: HashMap<&'static str, Vec<agent::CommandInfo>>,
    probing: HashSet<&'static str>,
}

impl Discovery {
    pub fn catalog(&self, agent: &str) -> Option<&SessionOptions> {
        self.catalogs.get(agent)
    }

    pub fn commands(&self, agent: &str) -> Option<&[agent::CommandInfo]> {
        self.commands.get(agent).map(Vec::as_slice)
    }

    pub fn is_probing(&self, agent: &str) -> bool {
        self.probing.contains(agent)
    }

    /// Registry agents with no catalog and no probe out: never probed, or
    /// probed and failed — which is what makes a failure retryable.
    pub(super) fn missing(&self) -> Vec<&'static str> {
        agent::AGENTS
            .iter()
            .map(|a| a.id)
            .filter(|id| !self.catalogs.contains_key(id) && !self.probing.contains(id))
            .collect()
    }

    /// Mark `agents` as being probed, returning the ones that were not already:
    /// two windows asking at once share one probe.
    pub(super) fn start(&mut self, agents: &[&'static str]) -> Vec<&'static str> {
        agents
            .iter()
            .copied()
            .filter(|id| self.probing.insert(id))
            .collect()
    }

    /// Fold one probe report in, returning whether anything a view shows
    /// changed.
    pub(super) fn record(&mut self, ev: agent::Event) -> bool {
        match ev {
            agent::Event::Commands { agent, commands } => {
                self.commands.insert(agent, commands);
                true
            }
            agent::Event::ProbeFailed { agent } => self.probing.remove(agent),
            ev => {
                let Some((agent, options)) = SessionOptions::from_event(&ev) else {
                    return false;
                };
                self.probing.remove(agent);
                if !options.is_empty() {
                    self.catalogs.insert(agent, options);
                }
                true
            }
        }
    }

    /// A batch of probes is over, whether or not each one reported.
    pub(super) fn finish(&mut self, agents: &[&'static str]) {
        for id in agents {
            self.probing.remove(id);
        }
    }
}

/// Carry agent events from the backend thread's std channel onto one the GPUI
/// thread can await: a plain forwarder thread in between, which ends when
/// either side hangs up.
pub fn bridge(events: std::sync::mpsc::Receiver<agent::Event>) -> UnboundedReceiver<agent::Event> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        for ev in events {
            if tx.unbounded_send(ev).is_err() {
                break;
            }
        }
    });
    rx
}

/// Probe each agent with its launch config, returning the stream their
/// reports arrive on.
pub(super) fn probe(agents: Vec<(&'static str, agent::Config)>) -> UnboundedReceiver<agent::Event> {
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<agent::Event>();
    for (id, cfg) in agents {
        agent::probe(id, cfg, ev_tx.clone());
    }
    bridge(ev_rx)
}

#[cfg(test)]
mod tests {
    use super::{Discovery, SessionOptions};
    use crate::agent;

    fn options_event(agent: &'static str, models: Vec<agent::ModelInfo>) -> agent::Event {
        agent::Event::SessionOptions {
            agent,
            models,
            current_model: None,
            modes: Vec::new(),
            current_mode: None,
            access: Vec::new(),
            current_access: None,
            sandboxes: Vec::new(),
            current_sandbox: None,
            reasoning: Vec::new(),
            current_reasoning: None,
        }
    }

    fn one_model() -> Vec<agent::ModelInfo> {
        vec![agent::ModelInfo {
            value: "m".into(),
            name: "M".into(),
            provider: "p".into(),
        }]
    }

    /// Two windows asking for the same agent share one probe, and a cached
    /// agent is not asked for again unless someone refreshes it.
    #[test]
    fn a_probe_in_flight_is_not_started_twice() {
        let mut discovery = Discovery::default();
        assert_eq!(discovery.start(&["codex", "pi"]), ["codex", "pi"]);
        assert_eq!(discovery.start(&["codex", "claude"]), ["claude"]);
        assert!(!discovery.missing().contains(&"codex"), "in flight");

        discovery.record(options_event("codex", one_model()));
        assert!(!discovery.is_probing("codex"));
        assert!(discovery.catalog("codex").is_some());
        assert!(!discovery.missing().contains(&"codex"), "cached");
    }

    /// A failed probe leaves nothing cached and nothing in flight, so the next
    /// ask tries again.
    #[test]
    fn a_failed_probe_can_be_retried() {
        let mut discovery = Discovery::default();
        discovery.start(&["pi"]);
        discovery.record(agent::Event::ProbeFailed { agent: "pi" });

        assert!(discovery.missing().contains(&"pi"));
        assert_eq!(discovery.start(&["pi"]), ["pi"]);
    }

    /// An empty report is no catalog: the agent stays unknown rather than
    /// reading as one that offers nothing.
    #[test]
    fn an_empty_report_is_not_cached() {
        assert!(SessionOptions::default().is_empty());
        let mut discovery = Discovery::default();
        discovery.start(&["omp"]);
        discovery.record(options_event("omp", Vec::new()));
        assert!(discovery.catalog("omp").is_none());
        assert!(!discovery.is_probing("omp"));
    }
}
