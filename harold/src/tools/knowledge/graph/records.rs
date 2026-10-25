//! The records of the two graph logs, and the rules that make a name, a title or a relation
//! well formed. A record holds names, titles, kinds and relations, never a vault note's text.

use serde::{Deserialize, Serialize};

/// The relation types the first schema step defines.
pub(crate) const RELATION_TYPES: [&str; 5] = [
    "relates_to",
    "supersedes",
    "part_of",
    "inspired_by",
    "contradicts",
];

const MAX_NAME: usize = 300;
const MAX_TEXT: usize = 500;
const MAX_REASON: usize = 40;

/// One change to the graph, as the data log holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Op {
    PutVaultNote {
        name: String,
        title: String,
        kind: String,
    },
    MarkVaultNoteMissing {
        name: String,
    },
    RemoveVaultNote {
        name: String,
    },
    PutLink {
        from: String,
        to: String,
    },
    RemoveLink {
        from: String,
        to: String,
    },
    PutRelation {
        from: String,
        relation: String,
        to: String,
    },
    RemoveRelation {
        from: String,
        relation: String,
        to: String,
    },
    VoidRecord {
        sequence: u64,
        reason: String,
    },
}

/// Who asked for a change: the pass over the vault, or a call through the knowledge tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum By {
    Index,
    Call,
}

/// A data log record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DataRecord {
    #[serde(flatten)]
    pub op: Op,
    pub at_ms: i64,
    pub by: By,
}

/// A schema log record: the step's number and its TypeQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SchemaRecord {
    pub step: u32,
    pub at_ms: i64,
    pub define: String,
}

/// Why a change is not well formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Malformed {
    Relation,
    VaultNote,
    Reason,
}

fn plain(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

/// A vault note's name: the path of its file in the vault without `.md`, as `[[links]]` write it.
pub(crate) fn well_formed_name(name: &str) -> bool {
    plain(name, MAX_NAME)
        && !name.starts_with('/')
        && !name.contains("..")
        && !name.ends_with(".md")
}

pub(crate) fn known_relation(relation: &str) -> bool {
    RELATION_TYPES.contains(&relation)
}

impl Op {
    /// The first part of the check: every field is well formed. The graph itself is checked
    /// by the graph writer.
    pub(crate) fn well_formed(&self) -> Result<(), Malformed> {
        let names_ok = |names: &[&str]| {
            names
                .iter()
                .all(|name| well_formed_name(name))
                .then_some(())
                .ok_or(Malformed::VaultNote)
        };
        match self {
            Op::PutVaultNote { name, title, kind } => {
                names_ok(&[name])?;
                // A title may be empty in a note without one; the pass gives it the name.
                if title.len() > MAX_TEXT
                    || kind.len() > MAX_TEXT
                    || title.chars().any(char::is_control)
                    || kind.chars().any(char::is_control)
                {
                    return Err(Malformed::VaultNote);
                }
                Ok(())
            }
            Op::MarkVaultNoteMissing { name } | Op::RemoveVaultNote { name } => names_ok(&[name]),
            Op::PutLink { from, to } | Op::RemoveLink { from, to } => names_ok(&[from, to]),
            Op::PutRelation { from, relation, to } => {
                names_ok(&[from, to])?;
                if !known_relation(relation) || from == to {
                    return Err(Malformed::Relation);
                }
                Ok(())
            }
            Op::RemoveRelation { from, relation, to } => {
                names_ok(&[from, to])?;
                if !known_relation(relation) {
                    return Err(Malformed::Relation);
                }
                Ok(())
            }
            Op::VoidRecord { reason, .. } => {
                if plain(reason, MAX_REASON)
                    && reason
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    Ok(())
                } else {
                    Err(Malformed::Reason)
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod tests;
