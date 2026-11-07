//! Throwaway catalogue reads used by the settings probe.
//!
//! A live session used to own this loop; probing still needs the same reads
//! against a private `opencode serve`, without opening a prompt loop.

use std::time::Duration;

use crate::agent::event::CommandInfo;

use super::convert::{self, Catalog};
use super::transport::Server;

/// How long a fresh server may take to have its model catalogue ready: the
/// first read can answer an empty list while it is still loading.
const CATALOGUE_WAIT: Duration = Duration::from_secs(10);

/// Read models, modes and commands from a running server.
///
/// Models are polled until the list is non-empty, for a while: a fresh server
/// loads it lazily, answering the first read with nothing.
pub(super) async fn load_catalog(server: &Server) -> Result<(Catalog, Vec<CommandInfo>), String> {
    let models = async {
        let deadline = tokio::time::Instant::now() + CATALOGUE_WAIT;
        loop {
            let models = convert::model_entries(&server.get("/api/model").await?);
            if !models.is_empty() || tokio::time::Instant::now() >= deadline {
                break Ok::<_, String>(models);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    let (models, agents, commands, default) = tokio::join!(
        models,
        server.get("/api/agent"),
        server.get("/api/command"),
        server.get("/api/model/default"),
    );
    let modes = convert::mode_catalog(&agents.unwrap_or_default());
    let catalog = Catalog {
        models: models?,
        model: default
            .unwrap_or_default()
            .get("data")
            .and_then(convert::model_ref)
            .map(|(model, _)| model),
        mode: modes.first().map(|mode| mode.value.clone()),
        modes,
        variant: None,
    };
    Ok((
        catalog,
        convert::command_catalog(&commands.unwrap_or_default()),
    ))
}
