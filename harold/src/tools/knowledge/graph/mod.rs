//! The knowledge graph: vault notes, their links and the chief's typed relations, held in
//! TypeDB and rebuilt from two OpenData logs, a schema log and a data log. One graph writer
//! checks, logs and applies every change.

pub(crate) mod backend;
pub(crate) mod catchup;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod log;
pub(crate) mod metrics;
pub(crate) mod records;
pub(crate) mod runtime;
pub(crate) mod schema;
pub(crate) mod server;
pub(crate) mod sync;
pub(crate) mod typedb;
pub(crate) mod writer;
