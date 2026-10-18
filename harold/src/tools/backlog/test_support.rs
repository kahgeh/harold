//! Helpers shared by the backlog tool's tests.

use std::path::PathBuf;

use super::schema::Item;

/// A folder under the system temp folder, removed when dropped.
pub(super) struct TempFolder(pub PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-backlog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A valid item: title `Title <id>`, size `small`, one evidence pointer.
pub(super) fn item(id: &str) -> Item {
    Item {
        id: id.into(),
        title: format!("Title {id}"),
        value: String::new(),
        size: "small".into(),
        risk: String::new(),
        reason: String::new(),
        paths: String::new(),
        after: String::new(),
        evidence: vec!["harold.log:1".into()],
        rank: 0,
        updated_at_ms: 0,
    }
}
