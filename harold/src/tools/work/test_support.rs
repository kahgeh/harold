//! Helpers shared by the work tool's tests.

use std::path::PathBuf;

use super::schema::{Decision, Mark, Priority, Record, Status};

/// A folder under the system temp folder, removed when dropped.
pub(super) struct TempFolder(pub PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-work-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// The path of a work file of the given name in this folder.
    pub(super) fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new record: priority `later`, status `idea`, no decision, no link. The
/// store sets `created_in` and the times.
pub(super) fn record(name: &str) -> Record {
    Record {
        name: name.into(),
        title: format!("Title {name}"),
        priority: Priority::Later,
        status: Status::Idea,
        waits_for: String::new(),
        next_step: String::new(),
        created_in: String::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
        status_since_ms: 0,
        decisions: Vec::new(),
        links: Vec::new(),
    }
}

pub(super) fn decision(text: &str) -> Decision {
    Decision {
        at: "2026-11-03".into(),
        who: "the chief".into(),
        mark: Mark::Draft,
        text: text.into(),
    }
}
