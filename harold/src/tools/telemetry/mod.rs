//! The telemetry tool: agents declare and record counters, gauges and events
//! under a prefix. This part holds the names, the value lists and the store.
//! The service, the guard and the registration come in later tasks.

// Nothing outside the tests uses these modules until the service is added.
#![allow(dead_code)]

pub(crate) mod name;
pub(crate) mod schema;
pub(crate) mod store;
