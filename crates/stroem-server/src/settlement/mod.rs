//! Job settlement: everything a job owes after one of its steps moves, from
//! the step cascade through terminal handling. See
//! `docs/superpowers/specs/2026-09-08-job-settlement-design.md` and the
//! `### Settlement` section of CLAUDE.md.

pub mod settle;

pub use settle::{cascade_and_settle, settle_if_all_terminal, Settled};
