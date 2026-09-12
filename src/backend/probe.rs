//! Session option discovery, dispatched to the backend that owns the agent.
//!
//! A probe is a throwaway connection: it handshakes, reads what the agent
//! exposes (models, and modes when it has them), reports it, and returns —
//! nothing is kept alive, so no subprocess is left behind.

use std::sync::mpsc::Sender;

use super::config::{Backend, Config};
use super::event::Event;

/// Discover an agent's session options and report them as
/// [`Event::SessionOptions`], or [`Event::ProbeFailed`] when the agent is
/// missing or the handshake fails.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    match cfg.backend {
        Backend::Acp => super::acp::probe::probe(agent, cfg, ev_tx),
        Backend::Codex => super::codex::probe::probe(agent, cfg, ev_tx),
    }
}
