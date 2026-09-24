//! Session option discovery for the opencode backend.
//!
//! A throwaway server: it reads the catalogue a session here would start from —
//! the models (each with its own reasoning levels), the modes and the commands,
//! and the location's default model — reports it, and is dropped, which ends
//! the process. No session is created, so probing leaves nothing in opencode's
//! history.

use std::sync::mpsc::Sender;

use crate::agent::config::Config;
use crate::agent::event::{Event, emit};
use crate::agent::probe::run;

use super::session::load_catalog;
use super::transport::Server;

/// Probe a local `opencode` far enough to read its catalogue, then report it.
///
/// Run in the process's own directory, like the ACP probe was: what opencode
/// offers depends on the location's configuration, and this is the one a
/// session started without a project would see.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    run(agent, "opencode", ev_tx, move |ev_tx| async move {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let server = Server::start(&cfg.command, &cfg.env, &cwd).await?;
        let (catalog, commands) = load_catalog(&server).await?;
        emit(&ev_tx, catalog.options(agent));
        emit(&ev_tx, Event::Commands { agent, commands });
        Ok::<(), String>(())
    });
}
