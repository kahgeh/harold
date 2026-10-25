//! A graph in memory for tests: the same rules as the TypeDB client, no server.

use std::collections::BTreeSet;
use std::sync::Mutex;

use super::backend::*;
use super::records::{Op, SchemaRecord};

#[derive(Default)]
struct State {
    exists: bool,
    position: Position,
    snapshot: Snapshot,
    steps: Vec<u32>,
    /// Data sequences the fake refuses, as TypeDB would refuse a record it cannot take.
    refuse: BTreeSet<u64>,
    unreachable: bool,
    applied: Vec<u64>,
}

#[derive(Default)]
pub(crate) struct FakeGraph {
    state: Mutex<State>,
}

impl FakeGraph {
    pub(crate) fn refuse(&self, sequence: u64) {
        self.state.lock().unwrap().refuse.insert(sequence);
    }

    pub(crate) fn stop_refusing(&self) {
        self.state.lock().unwrap().refuse.clear();
    }

    pub(crate) fn set_unreachable(&self, unreachable: bool) {
        self.state.lock().unwrap().unreachable = unreachable;
    }

    /// The data sequences applied, in order, since the database was made.
    pub(crate) fn applied(&self) -> Vec<u64> {
        self.state.lock().unwrap().applied.clone()
    }

    pub(crate) fn lose_database(&self) {
        *self.state.lock().unwrap() = State::default();
    }

    pub(crate) fn schema_steps(&self) -> Vec<u32> {
        self.state.lock().unwrap().steps.clone()
    }

    fn lock(&self) -> BackendResult<std::sync::MutexGuard<'_, State>> {
        let state = self.state.lock().unwrap();
        if state.unreachable {
            return Err(BackendError::Unreachable);
        }
        Ok(state)
    }
}

fn changes(snapshot: &Snapshot, op: &Op) -> bool {
    match op {
        Op::PutVaultNote { name, title, kind } => snapshot
            .notes
            .get(name)
            .is_none_or(|note| note.title != *title || note.kind != *kind || note.missing),
        Op::MarkVaultNoteMissing { name } => snapshot.notes.get(name).is_some_and(|n| !n.missing),
        Op::RemoveVaultNote { name } => snapshot.notes.contains_key(name),
        Op::PutLink { from, to } => !snapshot.links.contains(&(from.clone(), to.clone())),
        Op::RemoveLink { from, to } => snapshot.links.contains(&(from.clone(), to.clone())),
        Op::PutRelation { from, relation, to } => {
            !snapshot
                .relations
                .contains(&(from.clone(), relation.clone(), to.clone()))
        }
        Op::RemoveRelation { from, relation, to } => {
            snapshot
                .relations
                .contains(&(from.clone(), relation.clone(), to.clone()))
        }
        Op::VoidRecord { .. } => false,
    }
}

#[tonic::async_trait]
impl GraphBackend for FakeGraph {
    async fn database_exists(&self) -> BackendResult<bool> {
        Ok(self.lock()?.exists)
    }

    async fn create_database(&self) -> BackendResult<()> {
        let mut state = self.lock()?;
        *state = State {
            exists: true,
            ..State::default()
        };
        Ok(())
    }

    async fn drop_database(&self) -> BackendResult<()> {
        *self.lock()? = State::default();
        Ok(())
    }

    async fn position(&self) -> BackendResult<Position> {
        Ok(self.lock()?.position)
    }

    async fn apply_schema(&self, record: &SchemaRecord, sequence: u64) -> BackendResult<()> {
        let mut state = self.lock()?;
        if !state.steps.contains(&record.step) {
            state.steps.push(record.step);
        }
        state.position.schema = Some(sequence);
        Ok(())
    }

    async fn apply(&self, op: &Op, sequence: u64) -> BackendResult<()> {
        let mut state = self.lock()?;
        if state.refuse.contains(&sequence) {
            return Err(BackendError::Refused("refused".into()));
        }
        let snapshot = &mut state.snapshot;
        match op {
            Op::PutVaultNote { name, title, kind } => {
                snapshot.notes.insert(
                    name.clone(),
                    NoteInfo {
                        title: title.clone(),
                        kind: kind.clone(),
                        missing: false,
                    },
                );
            }
            Op::MarkVaultNoteMissing { name } => {
                if let Some(note) = snapshot.notes.get_mut(name) {
                    note.missing = true;
                }
            }
            Op::RemoveVaultNote { name } => {
                snapshot.notes.remove(name);
                snapshot
                    .links
                    .retain(|(from, to)| from != name && to != name);
            }
            Op::PutLink { from, to } => {
                if !snapshot.notes.contains_key(from) || !snapshot.notes.contains_key(to) {
                    return Err(BackendError::Refused("no_vault_note".into()));
                }
                snapshot.links.insert((from.clone(), to.clone()));
            }
            Op::RemoveLink { from, to } => {
                snapshot.links.remove(&(from.clone(), to.clone()));
            }
            Op::PutRelation { from, relation, to } => {
                if !snapshot.notes.contains_key(from) || !snapshot.notes.contains_key(to) {
                    return Err(BackendError::Refused("no_vault_note".into()));
                }
                snapshot
                    .relations
                    .insert((from.clone(), relation.clone(), to.clone()));
            }
            Op::RemoveRelation { from, relation, to } => {
                snapshot
                    .relations
                    .remove(&(from.clone(), relation.clone(), to.clone()));
            }
            Op::VoidRecord { .. } => {}
        }
        state.position.data = Some(sequence);
        state.applied.push(sequence);
        Ok(())
    }

    async fn advance(&self, which: Which, sequence: u64) -> BackendResult<()> {
        let mut state = self.lock()?;
        match which {
            Which::Schema => state.position.schema = Some(sequence),
            Which::Data => state.position.data = Some(sequence),
        }
        Ok(())
    }

    async fn note_state(&self, name: &str) -> BackendResult<Option<NoteState>> {
        let state = self.lock()?;
        Ok(state.snapshot.notes.get(name).map(|note| NoteState {
            missing: note.missing,
            in_typed_relation: state
                .snapshot
                .relations
                .iter()
                .any(|(from, _, to)| from == name || to == name),
        }))
    }

    async fn would_change(&self, op: &Op) -> BackendResult<bool> {
        Ok(changes(&self.lock()?.snapshot, op))
    }

    async fn relations_of(&self, name: &str) -> BackendResult<Vec<RelationRow>> {
        let state = self.lock()?;
        let snapshot = &state.snapshot;
        let row = |other: &String, relation: &str, outgoing: bool| {
            let note = snapshot.notes.get(other);
            RelationRow {
                other: other.clone(),
                other_title: note.map(|n| n.title.clone()).unwrap_or_default(),
                relation: relation.into(),
                outgoing,
                other_missing: note.is_none_or(|n| n.missing),
            }
        };
        let mut rows = Vec::new();
        for (from, to) in &snapshot.links {
            if from == name {
                rows.push(row(to, "links", true));
            } else if to == name {
                rows.push(row(from, "links", false));
            }
        }
        for (from, relation, to) in &snapshot.relations {
            if from == name {
                rows.push(row(to, relation, true));
            } else if to == name {
                rows.push(row(from, relation, false));
            }
        }
        Ok(rows)
    }

    async fn snapshot(&self) -> BackendResult<Snapshot> {
        Ok(self.lock()?.snapshot.clone())
    }

    async fn stop(&self) {}
}
