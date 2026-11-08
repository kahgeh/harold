//! The factory flow page: eight flow stages, each with its items waiting, its median wait
//! and its levers. The numbers come from one named query that Harold reads from ClickHouse.

/// The named query, the same file that `bin/logq --named factory-flow` runs.
pub(crate) const FACTORY_FLOW_SQL: &str =
    include_str!("../../../docs/references/telemetry/queries/factory-flow.sql");
