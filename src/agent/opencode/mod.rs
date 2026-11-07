//! The opencode adapter: opencode's own v2 API, used for settings probes.
//!
//! `opencode serve` is an HTTP surface plus a server-sent event stream. A probe
//! starts a private server the way opencode's own CLI does for `--standalone`,
//! reads its catalogue, and drops it.

pub(super) mod catalog;
pub(super) mod convert;
pub(super) mod probe;

mod transport;
