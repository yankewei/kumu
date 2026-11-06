//! Session option discovery, dispatched to the backend that owns the agent.
//!
//! A probe is a throwaway connection: it handshakes, reads what the agent
//! exposes (models, and modes when it has them), reports it, and returns —
//! nothing is kept alive, so no subprocess is left behind.

use std::sync::mpsc::Sender;
use std::time::Duration;

use super::config::{Backend, Config};
use super::event::{Event, emit};

/// How long a probe may take before it is reported as failed.
///
/// Generous on purpose: an agent's first handshake can be slow, and it has to
/// outlast the slowest backend's own budget — opencode allows its server 30s
/// to start on a fresh machine, then 10s for the model catalogue. It is still
/// bounded, because a probe that reported nothing either way would leave
/// whoever asked for the options waiting with no verdict at all.
const TIMEOUT: Duration = Duration::from_secs(45);

/// Discover an agent's session options and report them as
/// [`Event::SessionOptions`], or [`Event::ProbeFailed`] when the agent is
/// missing or the handshake fails.
pub fn probe(agent: &'static str, cfg: Config, ev_tx: Sender<Event>) {
    match cfg.backend {
        Backend::Acp => super::acp::probe::probe(agent, cfg, ev_tx),
        Backend::Codex => super::codex::probe::probe(agent, cfg, ev_tx),
        Backend::Claude => super::claude::probe::probe(agent, cfg, ev_tx),
        Backend::Pi => super::pi::probe::probe(agent, cfg, ev_tx),
        Backend::Opencode => super::opencode::probe::probe(agent, cfg, ev_tx),
    }
}

/// Run one backend's probe `body` and report its outcome.
///
/// Every backend's probe is this same shape and differs only in what it asks,
/// so the shape lives here once:
///
///   - its own thread, because a probe must not block whoever asked for it —
///     the settings window calls this from the UI thread;
///   - its own tokio runtime, because the backends' transports are async and
///     there is no ambient one on that thread;
///   - a [`TIMEOUT`], because an agent that accepts the subprocess but never
///     answers must still be given a verdict.
///
/// A failing body and a timeout are both reported as [`Event::ProbeFailed`]:
/// to the caller they are one thing — the options could not be had — and a
/// missing binary and a silent handshake are equally unprobeable.
///
/// `tag` names the backend in the `KUMU_DEBUG` log line, matching the prefix
/// each backend's transport already uses. `body` is handed the event sender so
/// it can report the options it discovered.
pub(super) fn run<E, F, Fut>(agent: &'static str, tag: &'static str, ev_tx: Sender<Event>, body: F)
where
    E: std::fmt::Display,
    F: FnOnce(Sender<Event>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    std::thread::Builder::new()
        .name(format!("kumu-probe-{agent}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            // Built inside the timeout, so the body's own setup (paths,
            // launch config) counts against it too.
            let result =
                rt.block_on(async { tokio::time::timeout(TIMEOUT, body(ev_tx.clone())).await });
            let fail = |msg: String| {
                if std::env::var("KUMU_DEBUG").is_ok() {
                    eprintln!("[{tag}] probe {agent} failed: {msg}");
                }
                emit(&ev_tx, Event::ProbeFailed { agent });
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => fail(err.to_string()),
                Err(err) => fail(format!("timed out: {err}")),
            }
        })
        .expect("failed to spawn probe thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body that fails must become `ProbeFailed`, not silence: the caller has
    /// no other way to learn that the agent could not be probed.
    #[test]
    fn a_failing_body_becomes_probe_failed() {
        let (tx, rx) = std::sync::mpsc::channel();
        run("agent", "test", tx, |_| async {
            Err::<(), String>("boom".into())
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Event::ProbeFailed { agent }) => assert_eq!(agent, "agent"),
            _ => panic!("a failed body must report ProbeFailed"),
        }
    }

    /// A body that succeeds reports only what it emitted itself — the harness
    /// must not add an event of its own, or the caller would see a failure for
    /// a probe that worked.
    #[test]
    fn a_successful_body_reports_nothing_by_itself() {
        let (tx, rx) = std::sync::mpsc::channel();
        run("agent", "test", tx, |_| async { Ok::<(), String>(()) });
        assert!(
            !matches!(
                rx.recv_timeout(Duration::from_secs(30)),
                Ok(Event::ProbeFailed { .. })
            ),
            "a successful body must not be reported as a failure"
        );
    }
}
