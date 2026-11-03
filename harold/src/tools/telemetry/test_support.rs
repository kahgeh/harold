//! Helpers shared by the telemetry tool's tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::tools::{ChiefPane, PaneFolder, Products};

/// A folder under the system temp folder, removed when dropped.
pub(super) struct TempFolder(pub PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-telemetry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `PaneFolder` over a set map of pane id to folder.
#[derive(Default)]
pub(super) struct FakeFolders(pub Mutex<HashMap<String, PathBuf>>);

impl PaneFolder for FakeFolders {
    fn folder_of(&self, pane_id: &str) -> Option<PathBuf> {
        self.0.lock().unwrap().get(pane_id).cloned()
    }
}

/// A `Products` that returns the result it was given.
pub(super) struct FakeProducts(pub Result<Vec<(String, PathBuf)>, &'static str>);

impl Products for FakeProducts {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        self.0.clone()
    }
}

/// A `ChiefPane` with the given pane, or none.
pub(super) struct FakeChief(pub Option<String>);

impl ChiefPane for FakeChief {
    fn chief_pane(&self) -> Option<String> {
        self.0.clone()
    }
}
