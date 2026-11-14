//! The port that checks a details entry. The entry lives with the thing that the decision
//! belongs to (ADR for the decision tool), and Harold only reads it.

use std::path::PathBuf;

use crate::tools::BoxFuture;

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
