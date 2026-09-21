//! Test-only helpers shared by the end-to-end agent tests in `probe` and
//! `session`, which both drive a real local agent binary.
//!
//! Those tests are `#[ignore]`d. Driving a real agent reaches a service with the
//! machine's own credentials, spends the user's quota, and writes history into
//! the agent's own config directory, none of which a plain `cargo test` should
//! do — so finding the binary on `PATH` is not taken as consent. Run them with
//! `cargo test -- --ignored` when that is the intent.
//!
//! The wire is pinned without an agent: `schema/` holds captured traffic per
//! protocol and the parsing tests read those fixtures, so these end-to-end runs
//! exist to confirm the integration rather than to cover the formats.

/// The real-agent tests share one local agent binary; running its
/// subprocesses concurrently is slow enough to trip the timeouts, so
/// serialize them.
pub static AGENT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whether `command` resolves to an existing executable (a path, or on
/// PATH).
pub fn command_exists(command: &str) -> bool {
    if command.contains('/') {
        return std::path::Path::new(command).is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(command))
                .any(|candidate| candidate.is_file())
        })
        .unwrap_or(false)
}
