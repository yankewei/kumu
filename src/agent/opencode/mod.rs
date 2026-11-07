//! The opencode adapter: opencode's own v2 API, used for settings probes.
//!
//! A probe starts a private `opencode serve` the way opencode's own CLI does
//! for `--standalone`, reads its catalogue, and drops it.

pub(super) mod catalog;
pub(super) mod convert;
pub(super) mod probe;

mod transport;
