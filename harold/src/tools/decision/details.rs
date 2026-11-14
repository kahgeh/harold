//! The port that checks a details entry. The entry lives with the thing that the decision
//! belongs to (ADR for the decision tool), and Harold only reads it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::tools::{BoxFuture, Products};

/// Why a details entry is refused. The reason is one fixed word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailsError {
    pub reason: &'static str,
}

/// What a passed check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetailsRef {
    /// `pod`, `vault` or `personal`.
    pub place: String,
    /// The full path of the file that holds the entry.
    pub full_path: PathBuf,
}

pub(super) trait DetailsPort: Send + Sync {
    /// Reads the file that `details` (`<place>:<path>#<id>`) names, and checks that the
    /// heading `## <id>:` exists and that `owner` is right for the place.
    fn check<'a>(
        &'a self,
        details: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<DetailsRef, DetailsError>>;

    /// The full path of the file that `details` names, without a read. `None` when the
    /// place cannot be found.
    fn path_of(&self, details: &str) -> Option<PathBuf>;
}

/// The folder of a pod, inside the product's repository.
const POD_FOLDER: &str = crate::pod_stats::POD_FOLDER;

/// Reads the details files of the four places. It writes nothing.
pub(super) struct FsDetails {
    /// `telemetry.pod_registry` is set.
    pod_registry_set: bool,
    products: Arc<dyn Products>,
    /// `[knowledge] vault`.
    vault: Option<PathBuf>,
    /// `[decision] personal_repository`.
    personal: Option<PathBuf>,
}

/// Where a details value points, before any read.
struct Resolved {
    place: &'static str,
    /// The folder of the place.
    base: PathBuf,
    full: PathBuf,
    /// The details owner that this place needs.
    owner: String,
    id: String,
}

fn refuse(reason: &'static str) -> DetailsError {
    DetailsError { reason }
}

impl FsDetails {
    pub(super) fn new(
        pod_registry_set: bool,
        products: Arc<dyn Products>,
        vault: Option<PathBuf>,
        personal: Option<PathBuf>,
    ) -> Self {
        Self {
            pod_registry_set,
            products,
            vault,
            personal,
        }
    }

    fn resolve(&self, details: &str) -> Result<Resolved, DetailsError> {
        let (spec, id) = details.rsplit_once('#').unwrap_or((details, ""));
        let (place, path) = spec.split_once(':').ok_or(refuse("unknown_place"))?;
        let (place, base, owner, rel) = match place {
            "pod" => {
                if !self.pod_registry_set {
                    return Err(refuse("no_pod_registry"));
                }
                let products = self
                    .products
                    .products()
                    .map_err(|_| refuse("no_pod_registry"))?;
                let (product, rel) = path.split_once('/').unwrap_or((path, ""));
                let repo = products
                    .into_iter()
                    .find(|(name, _)| name == product)
                    .map(|(_, repo)| repo)
                    .ok_or(refuse("unknown_product"))?;
                ("pod", repo.join(POD_FOLDER), format!("pnem:{product}"), rel)
            }
            "vault" => {
                let vault = self.vault.clone().ok_or(refuse("no_vault"))?;
                ("vault", vault, "chief".to_owned(), path)
            }
            "personal" => {
                let personal = self
                    .personal
                    .clone()
                    .ok_or(refuse("no_personal_repository"))?;
                ("personal", personal, "chief".to_owned(), path)
            }
            _ => return Err(refuse("unknown_place")),
        };
        let inside = Path::new(rel)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        if rel.starts_with('/') || !inside {
            return Err(refuse("path_outside_place"));
        }
        if rel.is_empty() {
            return Err(refuse("details_unreadable"));
        }
        Ok(Resolved {
            place,
            full: base.join(rel),
            base,
            owner,
            id: id.to_owned(),
        })
    }
}

impl DetailsPort for FsDetails {
    fn check<'a>(
        &'a self,
        details: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<DetailsRef, DetailsError>> {
        Box::pin(async move {
            let found = self.resolve(details)?;
            let unreadable = |_| refuse("details_unreadable");
            let real = tokio::fs::canonicalize(&found.full)
                .await
                .map_err(unreadable)?;
            let real_base = tokio::fs::canonicalize(&found.base)
                .await
                .map_err(unreadable)?;
            if !real.starts_with(&real_base) {
                return Err(refuse("path_outside_place"));
            }
            let text = tokio::fs::read_to_string(&real).await.map_err(unreadable)?;
            let heading = format!("## {}:", found.id);
            if found.id.is_empty() || !text.lines().any(|line| line.starts_with(&heading)) {
                return Err(refuse("details_missing"));
            }
            if owner != found.owner {
                return Err(refuse("details_owner_mismatch"));
            }
            Ok(DetailsRef {
                place: found.place.to_owned(),
                full_path: found.full,
            })
        })
    }

    fn path_of(&self, details: &str) -> Option<PathBuf> {
        self.resolve(details).ok().map(|found| found.full)
    }
}

#[cfg(test)]
#[path = "details_tests.rs"]
mod tests;
