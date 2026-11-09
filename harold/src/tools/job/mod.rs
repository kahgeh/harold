//! The job tool: an agent records a long command that it started outside its pane.
//! Harold checks the process, and types one end line into the agent's pane when the
//! process ends. Outside this module only `NAME` and `start` are used.
//!
//! An agent job is not a background job of Harold's own threads.

pub(crate) mod process;
mod service;
mod store;
mod validate;

pub(crate) const NAME: &str = "job";
