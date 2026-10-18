//! Who may write: a product's backlog is written from inside that product's
//! folder or by the chief; the queue only by the chief.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tools::{ChiefPane, PaneFolder, Products};

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    NoPane,
    UnknownProduct,
    WrongProduct,
    NotChief,
    RegistryUnreadable,
}

impl Refused {
    /// The fixed word the refusal goes by in a status message and a log line.
    pub(super) fn reason(self) -> &'static str {
        match self {
            Refused::NoPane => "no_pane",
            Refused::UnknownProduct => "unknown_product",
            Refused::WrongProduct => "wrong_product",
            Refused::NotChief => "not_chief",
            Refused::RegistryUnreadable => "registry_unreadable",
        }
    }
}

pub(super) struct Guard {
    pane_folder: Arc<dyn PaneFolder>,
    products: Arc<dyn Products>,
    chief_pane: Arc<dyn ChiefPane>,
}

impl Guard {
    pub(super) fn new(
        pane_folder: Arc<dyn PaneFolder>,
        products: Arc<dyn Products>,
        chief_pane: Arc<dyn ChiefPane>,
    ) -> Self {
        Self {
            pane_folder,
            products,
            chief_pane,
        }
    }

    /// The pane may write `product`'s backlog: it is the chief, or it sits in the
    /// product's repo folder.
    pub(super) fn may_write_product(
        &self,
        caller_pane: &str,
        product: &str,
    ) -> Result<(), Refused> {
        if caller_pane.is_empty() {
            return Err(Refused::NoPane);
        }
        let repo = self
            .products()?
            .into_iter()
            .find(|(name, _)| name == product)
            .map(|(_, repo)| repo)
            .ok_or(Refused::UnknownProduct)?;
        if self.is_chief(caller_pane) {
            return Ok(());
        }
        let folder = self
            .pane_folder
            .folder_of(caller_pane)
            .ok_or(Refused::WrongProduct)?;
        if canonical(&folder).starts_with(canonical(&repo)) {
            return Ok(());
        }
        Err(Refused::WrongProduct)
    }

    /// Only the chief writes the queue.
    pub(super) fn may_write_queue(&self, caller_pane: &str) -> Result<(), Refused> {
        if caller_pane.is_empty() {
            return Err(Refused::NoPane);
        }
        if self.is_chief(caller_pane) {
            return Ok(());
        }
        Err(Refused::NotChief)
    }

    /// For a dry-run import, which needs no pane: the product is in the registry.
    pub(super) fn product_known(&self, product: &str) -> Result<(), Refused> {
        match self.products()?.iter().any(|(name, _)| name == product) {
            true => Ok(()),
            false => Err(Refused::UnknownProduct),
        }
    }

    /// The registry's products as `(name, repo)`.
    pub(super) fn products(&self) -> Result<Vec<(String, PathBuf)>, Refused> {
        self.products
            .products()
            .map_err(|_| Refused::RegistryUnreadable)
    }

    fn is_chief(&self, pane: &str) -> bool {
        self.chief_pane.chief_pane().as_deref() == Some(pane)
    }
}

/// The path with symlinks and a trailing slash resolved; as given when it cannot be.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
