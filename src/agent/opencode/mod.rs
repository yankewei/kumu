//! The opencode backend: opencode's own v2 API.
//!
//! Not ACP, though opencode still speaks that too: `opencode serve` is its v2
//! API, an HTTP surface plus a server-sent event stream. Each session launches
//! a private server the way opencode's own CLI does for `--standalone`, bound to
//! a random local port and a random password, and alive exactly as long as its
//! stdin — so it lives and dies with the connection like every other backend's
//! subprocess. The same contract with the rest of kumu: it produces [`Event`]s
//! and consumes [`Cmd`]s, and nothing above `agent` knows which agent is
//! talking.
//!
//! Layout:
//!   - `transport` — launching the server, HTTP requests, the event stream;
//!   - `convert`   — protocol -> app translation, where protocol shapes stop;
//!   - `session`   — the session, the event pump, and the command loop;
//!   - `probe`     — session-option discovery for the settings window.
//!
//! The wire was checked against a live server and pinned under
//! `schema/opencode-v2/`: the server describes its requests (`openapi.json`)
//! but not its events, so those are captured traffic.
//!
//! [`Event`]: crate::agent::Event
//! [`Cmd`]: crate::agent::Cmd

pub(super) mod convert;
pub(super) mod probe;
pub(super) mod session;

mod transport;
