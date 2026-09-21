//! The application layer: what kumu does with agents, independent of how it is
//! drawn. The UI renders and reads this; the agent layer below it speaks the
//! protocols.
//!
//!   - `turn` — a live turn as plain data, and how agent events fold into it.

pub mod turn;
