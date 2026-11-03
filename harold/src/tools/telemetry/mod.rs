//! The telemetry tool: agents declare and record counters, gauges and events
//! under a prefix. This part holds the names, the value lists, the store, the
//! guard and the service. The start and the registration come in a later task.

// Nothing starts the service until the tool is registered (a later task).
#![allow(dead_code)]

pub(crate) mod guard;
pub(crate) mod name;
pub(crate) mod schema;
pub(crate) mod service;
pub(crate) mod signals;
pub(crate) mod store;

#[cfg(test)]
mod test_support;
