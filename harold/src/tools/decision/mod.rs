//! The decision tool: the records of decisions that a role made from evidence, each with a
//! check date, so the coach can grade them later. Outside this module only `NAME` and
//! `start` are used.
//!
//! A decision record is not a work record decision (ADR 0028): it never rotates.

#[allow(dead_code)]
mod store;
