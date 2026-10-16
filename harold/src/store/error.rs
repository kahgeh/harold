//! The store's one error type, for the event log and `harold-state.db` alike.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("state database: {0}")]
    State(#[from] turso::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("event log: {0}")]
    Log(String),

    /// Another Harold holds the store folder's lock.
    #[error("store is in use by another Harold")]
    InUse,

    /// A stored value Harold cannot read, or a schema it does not know.
    #[error("{0}")]
    Data(String),

    /// The store is failing: a write took too long, or a delivery write to the state
    /// database failed, and the store has not recovered since.
    #[error("store is failing")]
    Failing(FailedWrite),

    /// The task that owns an operation has stopped.
    #[error("{0}")]
    Stopped(&'static str),
}

/// How a write to a failing store ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedWrite {
    /// The write started and did not finish in time. It may still land later.
    TimedOut,
    /// The write was refused at once and never started.
    Refused,
}

pub type Result<T> = std::result::Result<T, StoreError>;
