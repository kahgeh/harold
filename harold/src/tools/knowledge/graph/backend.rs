//! What the graph writer needs of the graph database. The TypeDB client implements it; tests
//! use a fake that lives in memory.

use std::collections::{BTreeMap, BTreeSet};

use super::records::{Op, SchemaRecord};

/// The sequence of the last schema record and of the last data record applied. A sequence,
/// never a count: sequences rise but can jump.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Position {
    pub schema: Option<u64>,
    pub data: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Which {
    Schema,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendError {
    /// The database refused the record. The reason is one word.
    Refused(String),
    /// The server could not be reached or did not answer.
    Unreachable,
}

pub(crate) type BackendResult<T> = Result<T, BackendError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteInfo {
    pub title: String,
    pub kind: String,
    pub missing: bool,
}

/// A vault note as the check sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoteState {
    pub missing: bool,
    pub in_typed_relation: bool,
}

/// Everything in the graph: the vault notes by name, the links and the typed relations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub notes: BTreeMap<String, NoteInfo>,
    pub links: BTreeSet<(String, String)>,
    pub relations: BTreeSet<(String, String, String)>,
}

/// One relation of a vault note, in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationRow {
    pub other: String,
    pub other_title: String,
    /// `links` for a link in a vault note's text, else the typed relation's type.
    pub relation: String,
    /// True when the asked vault note is the source.
    pub outgoing: bool,
    pub other_missing: bool,
}

#[tonic::async_trait]
pub(crate) trait GraphBackend: Send + Sync {
    async fn database_exists(&self) -> BackendResult<bool>;
    /// Creates the empty database. The first schema step makes the graph position.
    async fn create_database(&self) -> BackendResult<()>;
    async fn drop_database(&self) -> BackendResult<()>;
    async fn position(&self) -> BackendResult<Position>;
    /// Defines a schema step and moves the schema position, in one transaction.
    async fn apply_schema(&self, record: &SchemaRecord, sequence: u64) -> BackendResult<()>;
    /// Applies a data record and moves the data position, in one transaction. Applying a
    /// record twice, or removing what is not there, changes nothing and is not an error.
    async fn apply(&self, op: &Op, sequence: u64) -> BackendResult<()>;
    /// Moves a position without a change, to pass a voided record.
    async fn advance(&self, which: Which, sequence: u64) -> BackendResult<()>;
    async fn note_state(&self, name: &str) -> BackendResult<Option<NoteState>>;
    /// Whether applying `op` would change the graph.
    async fn would_change(&self, op: &Op) -> BackendResult<bool>;
    async fn relations_of(&self, name: &str) -> BackendResult<Vec<RelationRow>>;
    async fn snapshot(&self) -> BackendResult<Snapshot>;
}
