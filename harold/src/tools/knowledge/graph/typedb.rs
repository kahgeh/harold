//! The TypeDB client: the `GraphBackend` over TypeDB's HTTP API, through the one `reqwest`
//! Harold already has. Every change is one transaction that also moves the graph position.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};

use super::backend::*;
use super::records::{Op, RELATION_TYPES, SchemaRecord};
use super::runtime::BackendOpener;
use super::server::{ServerConfig, TypeDbServer};
use super::writer::GraphError;

const DATABASE: &str = "harold_knowledge";
const POSITION_ID: &str = "main";
/// The answer limit of a read. TypeDB stops at 10,000 answers unless told more.
const ANSWER_LIMIT: u64 = 10_000_000;
/// The relation type of a link in a vault note's text. `links` is a TypeQL keyword.
const LINK_TYPE: &str = "note_link";

pub(crate) struct TypeDbBackend {
    server: Arc<TypeDbServer>,
}

impl TypeDbBackend {
    pub(crate) fn new(server: Arc<TypeDbServer>) -> Self {
        Self { server }
    }
}

/// A TypeQL string literal.
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One statement of a change. `must_match` is set when a statement that matches no row means a
/// vault note is not in the graph.
pub(crate) struct Statement {
    pub text: String,
    pub must_match: bool,
}

fn plain(text: String) -> Statement {
    Statement {
        text,
        must_match: false,
    }
}

fn matched(text: String) -> Statement {
    Statement {
        text,
        must_match: true,
    }
}

fn note(var: &str, name: &str) -> String {
    format!("${var} isa vault_note, has path {}", quote(name))
}

fn relation_type(relation: &str) -> Result<&str, BackendError> {
    if RELATION_TYPES.contains(&relation) {
        Ok(relation)
    } else {
        Err(BackendError::Refused("invalid_relation".into()))
    }
}

fn pair(kind: &str, from: &str, to: &str) -> String {
    format!(
        "{}; {}; $r isa {kind} (source: $a, target: $b);",
        note("a", from),
        note("b", to)
    )
}

/// The TypeQL statements that apply one data record, without the position.
pub(crate) fn statements(op: &Op) -> Result<Vec<Statement>, BackendError> {
    Ok(match op {
        Op::PutVaultNote { name, title, kind } => {
            let drop = |attribute: &str| {
                plain(format!(
                    "match {}, has {attribute} $x; delete has $x of $n;",
                    note("n", name)
                ))
            };
            vec![
                plain(format!("put {};", note("n", name))),
                drop("title"),
                drop("kind"),
                drop("missing"),
                plain(format!(
                    "match {}; insert $n has title {}, has kind {}, has missing false;",
                    note("n", name),
                    quote(title),
                    quote(kind)
                )),
            ]
        }
        Op::MarkVaultNoteMissing { name } => vec![
            plain(format!(
                "match {}, has missing $x; delete has $x of $n;",
                note("n", name)
            )),
            plain(format!(
                "match {}; insert $n has missing true;",
                note("n", name)
            )),
        ],
        Op::RemoveVaultNote { name } => vec![
            plain(format!(
                "match {}; $r isa {LINK_TYPE}, links ($n); delete $r;",
                note("n", name)
            )),
            plain(format!("match {}; delete $n;", note("n", name))),
        ],
        Op::PutLink { from, to } => vec![matched(format!(
            "match {}; {}; put $r isa {LINK_TYPE} (source: $a, target: $b);",
            note("a", from),
            note("b", to)
        ))],
        Op::RemoveLink { from, to } => vec![plain(format!(
            "match {} delete $r;",
            pair(LINK_TYPE, from, to)
        ))],
        Op::PutRelation { from, relation, to } => vec![matched(format!(
            "match {}; {}; put $r isa {} (source: $a, target: $b);",
            note("a", from),
            note("b", to),
            relation_type(relation)?
        ))],
        Op::RemoveRelation { from, relation, to } => vec![plain(format!(
            "match {} delete $r;",
            pair(relation_type(relation)?, from, to)
        ))],
        Op::VoidRecord { .. } => Vec::new(),
    })
}

/// The statements that move one position to `sequence`.
pub(crate) fn position_statements(which: Which, sequence: u64) -> Vec<String> {
    let attribute = match which {
        Which::Schema => "schema_sequence",
        Which::Data => "data_sequence",
    };
    let id = quote(POSITION_ID);
    vec![
        format!("put $g isa graph_position, has position_id {id};"),
        format!(
            "match $g isa graph_position, has position_id {id}, has {attribute} $x; delete has $x of $g;"
        ),
        format!(
            "match $g isa graph_position, has position_id {id}; insert $g has {attribute} {sequence};"
        ),
    ]
}

/// A TypeDB error body's code as the one-word reason, such as `cnt9`.
fn reason(body: &Value) -> String {
    body["code"]
        .as_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "server_error".into())
}

struct Reply {
    status: u16,
    body: Value,
}

/// What a reply that is not a success means. Only a 400 that carries a TypeDB code says the
/// database refused the query. Any other status (sign-in, missing database, server error) is
/// a fault of the server, not of a record, so it must never make the graph behind.
fn refusal(reply: &Reply) -> BackendError {
    if reply.status == 400 && reply.body["code"].is_string() {
        BackendError::Refused(reason(&reply.body))
    } else {
        BackendError::Unreachable
    }
}

impl TypeDbBackend {
    /// One request. A token the server refuses is replaced once.
    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> BackendResult<Reply> {
        for attempt in 0..2 {
            let lease = self.server.lease().await?;
            let mut request = self
                .server
                .client()
                .request(method.clone(), format!("{}{path}", lease.base))
                .bearer_auth(&lease.token);
            if let Some(body) = body {
                request = request.json(body);
            }
            let reply = request
                .send()
                .await
                .map_err(|_| BackendError::Unreachable)?;
            let status = reply.status().as_u16();
            if status == 401 && attempt == 0 {
                self.server.forget_token().await;
                continue;
            }
            let bytes = reply.bytes().await.map_err(|_| BackendError::Unreachable)?;
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Ok(Reply { status, body });
        }
        Err(BackendError::Unreachable)
    }

    async fn ok(&self, method: Method, path: &str, body: Option<&Value>) -> BackendResult<Reply> {
        let reply = self.send(method, path, body).await?;
        match reply.status {
            200..=299 => Ok(reply),
            _ => Err(refusal(&reply)),
        }
    }

    /// The rows a read returns, each as the map of its selected variables.
    async fn rows(&self, query: &str) -> BackendResult<Vec<Value>> {
        let body = json!({
            "query": query,
            "commit": false,
            "databaseName": DATABASE,
            "transactionType": "read",
            "queryOptions": {"includeInstanceTypes": false, "answerCountLimit": ANSWER_LIMIT},
        });
        let reply = self.send(Method::POST, "/v1/query", Some(&body)).await?;
        match reply.status {
            200 => Ok(reply.body["answers"]
                .as_array()
                .map(|rows| rows.iter().map(|row| row["data"].clone()).collect())
                .unwrap_or_default()),
            206 => Err(BackendError::Unreachable),
            _ => Err(refusal(&reply)),
        }
    }

    /// Runs the statements and the position statements in one transaction and commits it. A
    /// statement that must match and does not ends the transaction without a commit.
    async fn write(
        &self,
        kind: &str,
        statements: &[Statement],
        position: &[String],
    ) -> BackendResult<()> {
        let open = json!({"databaseName": DATABASE, "transactionType": kind});
        let opened = self
            .ok(Method::POST, "/v1/transactions/open", Some(&open))
            .await?;
        let Some(id) = opened.body["transactionId"].as_str().map(str::to_string) else {
            return Err(BackendError::Unreachable);
        };
        let result = self.run(&id, statements, position).await;
        if result.is_err() {
            let _ = self
                .send(Method::POST, &format!("/v1/transactions/{id}/close"), None)
                .await;
        }
        result
    }

    async fn run(
        &self,
        id: &str,
        statements: &[Statement],
        position: &[String],
    ) -> BackendResult<()> {
        let query_path = format!("/v1/transactions/{id}/query");
        let options = json!({"includeInstanceTypes": false});
        for statement in statements {
            let body = json!({"query": statement.text, "queryOptions": options});
            let reply = self.ok(Method::POST, &query_path, Some(&body)).await?;
            let rows = reply.body["answers"].as_array().map_or(0, Vec::len);
            if statement.must_match && rows == 0 {
                return Err(BackendError::Refused("no_vault_note".into()));
            }
        }
        for text in position {
            let body = json!({"query": text, "queryOptions": options});
            self.ok(Method::POST, &query_path, Some(&body)).await?;
        }
        self.ok(Method::POST, &format!("/v1/transactions/{id}/commit"), None)
            .await?;
        Ok(())
    }
}

fn text(row: &Value, variable: &str) -> String {
    row[variable]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn flag(row: &Value, variable: &str) -> bool {
    row[variable]["value"].as_bool().unwrap_or(false)
}

fn number(row: &Value, variable: &str) -> Option<u64> {
    row[variable]["value"].as_u64()
}

const NOTE_ROWS: &str = "match $n isa vault_note, has path $p, has title $t, has kind $k, has missing $m; select $p, $t, $k, $m;";

#[tonic::async_trait]
impl GraphBackend for TypeDbBackend {
    async fn database_exists(&self) -> BackendResult<bool> {
        let reply = self.ok(Method::GET, "/v1/databases", None).await?;
        Ok(reply.body["databases"]
            .as_array()
            .is_some_and(|names| names.iter().any(|name| name["name"] == DATABASE)))
    }

    async fn create_database(&self) -> BackendResult<()> {
        self.ok(Method::POST, &format!("/v1/databases/{DATABASE}"), None)
            .await
            .map(|_| ())
    }

    async fn drop_database(&self) -> BackendResult<()> {
        if !self.database_exists().await? {
            return Ok(());
        }
        self.ok(Method::DELETE, &format!("/v1/databases/{DATABASE}"), None)
            .await
            .map(|_| ())
    }

    async fn position(&self) -> BackendResult<Position> {
        let id = quote(POSITION_ID);
        let read = |attribute: &'static str| {
            let query = format!(
                "match $g isa graph_position, has position_id {id}, has {attribute} $x; select $x;"
            );
            async move {
                match self.rows(&query).await {
                    Ok(rows) => Ok(rows.first().and_then(|row| number(row, "x"))),
                    // The schema has no step yet, so the type is not there.
                    Err(BackendError::Refused(code)) if code == "inf2" => Ok(None),
                    Err(error) => Err(error),
                }
            }
        };
        Ok(Position {
            schema: read("schema_sequence").await?,
            data: read("data_sequence").await?,
        })
    }

    async fn apply_schema(&self, record: &SchemaRecord, sequence: u64) -> BackendResult<()> {
        let define = [plain(record.define.clone())];
        self.write(
            "schema",
            &define,
            &position_statements(Which::Schema, sequence),
        )
        .await
    }

    async fn apply(&self, op: &Op, sequence: u64) -> BackendResult<()> {
        self.write(
            "write",
            &statements(op)?,
            &position_statements(Which::Data, sequence),
        )
        .await
    }

    async fn advance(&self, which: Which, sequence: u64) -> BackendResult<()> {
        let kind = if which == Which::Schema {
            "schema"
        } else {
            "write"
        };
        self.write(kind, &[], &position_statements(which, sequence))
            .await
    }

    async fn note_state(&self, name: &str) -> BackendResult<Option<NoteState>> {
        let rows = self
            .rows(&format!(
                "match {}, has missing $m; select $m;",
                note("n", name)
            ))
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let typed = self
            .rows(&format!(
                "match {}; $r links ($n); not {{ $r isa {LINK_TYPE}; }}; select $r;",
                note("n", name)
            ))
            .await?;
        Ok(Some(NoteState {
            missing: flag(row, "m"),
            in_typed_relation: !typed.is_empty(),
        }))
    }

    async fn would_change(&self, op: &Op) -> BackendResult<bool> {
        let exists =
            |kind: &str, from: &str, to: &str| format!("match {} select $r;", pair(kind, from, to));
        Ok(match op {
            Op::PutVaultNote { name, title, kind } => {
                let rows = self
                    .rows(&format!(
                        "match {}, has title $t, has kind $k, has missing $m; select $t, $k, $m;",
                        note("n", name)
                    ))
                    .await?;
                !rows.first().is_some_and(|row| {
                    text(row, "t") == *title && text(row, "k") == *kind && !flag(row, "m")
                })
            }
            Op::MarkVaultNoteMissing { name } => !self
                .rows(&format!(
                    "match {}, has missing false; select $n;",
                    note("n", name)
                ))
                .await?
                .is_empty(),
            Op::RemoveVaultNote { name } => !self
                .rows(&format!("match {}; select $n;", note("n", name)))
                .await?
                .is_empty(),
            Op::PutLink { from, to } => self.rows(&exists(LINK_TYPE, from, to)).await?.is_empty(),
            Op::RemoveLink { from, to } => {
                !self.rows(&exists(LINK_TYPE, from, to)).await?.is_empty()
            }
            Op::PutRelation { from, relation, to } => self
                .rows(&exists(relation_type(relation)?, from, to))
                .await?
                .is_empty(),
            Op::RemoveRelation { from, relation, to } => !self
                .rows(&exists(relation_type(relation)?, from, to))
                .await?
                .is_empty(),
            Op::VoidRecord { .. } => false,
        })
    }

    async fn relations_of(&self, name: &str) -> BackendResult<Vec<RelationRow>> {
        let mut found = Vec::new();
        for kind in std::iter::once(LINK_TYPE).chain(RELATION_TYPES) {
            for outgoing in [true, false] {
                let (mine, theirs) = if outgoing {
                    ("source", "target")
                } else {
                    ("target", "source")
                };
                let rows = self
                    .rows(&format!(
                        "match {}; $r isa {kind} ({mine}: $n, {theirs}: $o); $o has path $p, has title $t, has missing $m; select $p, $t, $m;",
                        note("n", name)
                    ))
                    .await?;
                found.extend(rows.iter().map(|row| RelationRow {
                    other: text(row, "p"),
                    other_title: text(row, "t"),
                    relation: if kind == LINK_TYPE {
                        "links".into()
                    } else {
                        kind.into()
                    },
                    outgoing,
                    other_missing: flag(row, "m"),
                }));
            }
        }
        found.sort_by(|a, b| {
            (&a.relation, &a.other, !a.outgoing).cmp(&(&b.relation, &b.other, !b.outgoing))
        });
        Ok(found)
    }

    async fn snapshot(&self) -> BackendResult<Snapshot> {
        let mut snapshot = Snapshot::default();
        for row in self.rows(NOTE_ROWS).await? {
            snapshot.notes.insert(
                text(&row, "p"),
                NoteInfo {
                    title: text(&row, "t"),
                    kind: text(&row, "k"),
                    missing: flag(&row, "m"),
                },
            );
        }
        let ends = |kind: &str| {
            format!(
                "match $r isa {kind} (source: $a, target: $b); $a has path $x; $b has path $y; select $x, $y;"
            )
        };
        for row in self.rows(&ends(LINK_TYPE)).await? {
            snapshot.links.insert((text(&row, "x"), text(&row, "y")));
        }
        for kind in RELATION_TYPES {
            for row in self.rows(&ends(kind)).await? {
                snapshot
                    .relations
                    .insert((text(&row, "x"), kind.to_string(), text(&row, "y")));
            }
        }
        Ok(snapshot)
    }
}

/// Makes the TypeDB client on the first graph call. The server starts on its first request and
/// stops after the idle time or at shutdown.
pub(crate) struct TypeDbOpener {
    pub binary: PathBuf,
    pub idle: Duration,
    pub shutdown: tokio::sync::watch::Receiver<()>,
    /// The one server of this opener, so a second open never starts a second server.
    pub server: std::sync::Mutex<Option<Arc<TypeDbServer>>>,
}

#[tonic::async_trait]
impl BackendOpener for TypeDbOpener {
    async fn open(&self, typedb_folder: &Path) -> Result<Arc<dyn GraphBackend>, GraphError> {
        let mut held = self
            .server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = match &*held {
            Some(server) => Arc::clone(server),
            None => {
                let server = TypeDbServer::new(ServerConfig {
                    binary: self.binary.clone(),
                    folder: typedb_folder.to_path_buf(),
                    idle: self.idle,
                });
                drop(server.spawn_reaper(self.shutdown.clone()));
                *held = Some(Arc::clone(&server));
                server
            }
        };
        Ok(Arc::new(TypeDbBackend::new(server)))
    }
}

#[cfg(test)]
#[path = "typedb_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "typedb_live_tests.rs"]
mod live_tests;
