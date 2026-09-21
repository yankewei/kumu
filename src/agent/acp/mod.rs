//! The ACP backend: the Agent Client Protocol v1 over stdio.
//!
//! Everything in here is ACP-specific. The vocabulary the UI consumes, and the
//! launch/connection machinery shared with the other backends, live one level
//! up in `backend`.
//!
//! Layout:
//!   - `session` — the live session loop and its two handlers;
//!   - `convert` — protocol -> app translation, where protocol types stop;
//!   - `probe`   — the throwaway connection that discovers session options.

pub(super) mod convert;
pub(super) mod probe;
pub(super) mod session;
