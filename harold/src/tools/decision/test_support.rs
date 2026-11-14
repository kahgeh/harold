//! Fake ports for the decision tool's tests. No test reads a real pane, pod folder, vault
//! or personal repository.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::details::{DetailsError, DetailsPort, DetailsRef};
use super::store::DecisionStore;
use crate::tools::{BoxFuture, ChiefPane};

/// A folder under the system's temporary folder, removed when dropped.
pub(super) struct Temp(pub(super) PathBuf);

impl Temp {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-decision-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(super) async fn store(&self) -> Arc<DecisionStore> {
        Arc::new(DecisionStore::open(&self.0).await.unwrap())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The chief is the pane `%1`.
pub(super) const CHIEF: &str = "%1";

pub(super) struct FakeChief;

impl ChiefPane for FakeChief {
    fn chief_pane(&self) -> Option<String> {
        Some(CHIEF.to_owned())
    }
}

/// A details check that passes, or refuses with one reason, and keeps what it was asked.
#[derive(Default)]
pub(super) struct FakeDetails {
    pub(super) refuse_with: Mutex<Option<&'static str>>,
    pub(super) asked: Mutex<Vec<(String, String)>>,
}

impl DetailsPort for FakeDetails {
    fn check<'a>(
        &'a self,
        details: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<DetailsRef, DetailsError>> {
        self.asked
            .lock()
            .unwrap()
            .push((details.to_owned(), owner.to_owned()));
        let refused = *self.refuse_with.lock().unwrap();
        let place = details.split(':').next().unwrap_or_default().to_owned();
        Box::pin(async move {
            match refused {
                Some(reason) => Err(DetailsError { reason }),
                None => Ok(DetailsRef {
                    full_path: PathBuf::from(format!("/fixture/{place}/entry.md")),
                    place,
                }),
            }
        })
    }

    fn path_of(&self, details: &str) -> Option<PathBuf> {
        let place = details.split(':').next()?;
        Some(PathBuf::from(format!("/fixture/{place}/entry.md")))
    }
}
