//! The store's one error type, for the event log and `harold-state.db` alike.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("state database: {0}")]
    State(#[from] turso::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[allow(
        dead_code,
        reason = "the OpenData log reports its errors here from Task 2"
    )]
    #[error("event log: {0}")]
    Log(String),

    /// Another Harold holds the store folder's lock.
    #[allow(
        dead_code,
        reason = "the store folder's lock refuses a second Harold from Task 3"
    )]
    #[error("store is in use by another Harold")]
    InUse,

    /// A stored value Harold cannot read, or a schema it does not know.
    #[error("{0}")]
    Data(String),

    /// The task that owns an operation has stopped.
    #[error("{0}")]
    Stopped(&'static str),

    // Removed in Task 2 with the `events` crate.
    #[error(transparent)]
    Events(#[from] events::EsError),
}

pub type Result<T> = std::result::Result<T, StoreError>;
