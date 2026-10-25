//! The pass over the vault, as the graph sees it: put each vault note and its links, remove
//! the links that went, and for a vault note that went either remove it or, when a typed
//! relation names it, mark it missing. Only what differs from the graph is logged.

use std::collections::{BTreeMap, BTreeSet};

use super::backend::Snapshot;
use super::records::{By, Op};
use super::writer::{GraphError, GraphWriter};

/// A vault note as the pass read it from the vault.
pub(crate) struct Desired {
    pub name: String,
    pub title: String,
    pub kind: String,
    /// The targets of its `[[links]]`, as written.
    pub links: Vec<String>,
}

/// How many records the pass logged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Synced {
    pub logged: usize,
}

pub(crate) async fn sync_vault(
    writer: &GraphWriter,
    vault: &[Desired],
) -> Result<Synced, GraphError> {
    let snapshot = writer.snapshot().await?;
    let mut synced = Synced::default();
    let names: BTreeSet<&str> = vault.iter().map(|note| note.name.as_str()).collect();
    let notes: Vec<Op> = vault
        .iter()
        .filter(|note| {
            let title = title_of(note);
            !snapshot
                .notes
                .get(&note.name)
                .is_some_and(|held| held.title == title && held.kind == note.kind && !held.missing)
        })
        .map(|note| Op::PutVaultNote {
            name: note.name.clone(),
            title: title_of(note).to_string(),
            kind: note.kind.clone(),
        })
        .collect();

    // A link counts only when both vault notes are there.
    let wanted: BTreeSet<(String, String)> = vault
        .iter()
        .flat_map(|note| {
            note.links
                .iter()
                .filter(|target| names.contains(target.as_str()))
                .map(|target| (note.name.clone(), target.clone()))
        })
        .collect();
    let stale = snapshot
        .links
        .difference(&wanted)
        .map(|(from, to)| Op::RemoveLink {
            from: from.clone(),
            to: to.clone(),
        });
    let fresh = wanted
        .difference(&snapshot.links)
        .map(|(from, to)| Op::PutLink {
            from: from.clone(),
            to: to.clone(),
        });

    let ops = notes
        .into_iter()
        .chain(stale)
        .chain(fresh)
        .chain(gone_notes(&snapshot, &names));
    for op in ops {
        if writer.submit(op, By::Index).await?.is_some() {
            synced.logged += 1;
        }
    }
    Ok(synced)
}

fn title_of(note: &Desired) -> &str {
    if note.title.is_empty() {
        &note.name
    } else {
        &note.title
    }
}

/// A vault note that went: removed, or marked missing while a typed relation names it.
fn gone_notes(snapshot: &Snapshot, names: &BTreeSet<&str>) -> Vec<Op> {
    let named: BTreeMap<&str, bool> = snapshot
        .notes
        .keys()
        .map(|name| {
            let in_relation = snapshot
                .relations
                .iter()
                .any(|(from, _, to)| from == name || to == name);
            (name.as_str(), in_relation)
        })
        .collect();
    snapshot
        .notes
        .iter()
        .filter(|(name, _)| !names.contains(name.as_str()))
        .filter_map(|(name, held)| match named[name.as_str()] {
            true if held.missing => None,
            true => Some(Op::MarkVaultNoteMissing { name: name.clone() }),
            false => Some(Op::RemoveVaultNote { name: name.clone() }),
        })
        .collect()
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
