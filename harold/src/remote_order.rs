//! The remote order: the order of the PnEMs in the remote control.
//!
//! Harold keeps the order in `remote-order.json` in its data folder, as a JSON array of
//! product names. The file holds the names as the user set them. `get` checks them
//! against the registry each time, so a new product joins at the end and a removed
//! product drops out.

use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

const ORDER_FILE: &str = "remote-order.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteOrder {
    file: PathBuf,
}

impl RemoteOrder {
    pub(crate) fn new(data_folder: &Path) -> Self {
        Self {
            file: data_folder.join(ORDER_FILE),
        }
    }

    /// The order in the data folder that holds the store file at `store_path`: the same
    /// folder as `restart-reminder.json`. `None` when the path has no folder.
    pub(crate) fn in_data_folder_of(store_path: &str) -> Option<Self> {
        Path::new(store_path).parent().map(Self::new)
    }

    /// The product names in remote order. The names of the file that are in `registry`
    /// come first, in file order and once each. The other names of `registry` follow, in
    /// registry order. A file that cannot be read gives the registry order and one
    /// `warn` line.
    pub(crate) fn get(&self, registry: &[String]) -> Vec<String> {
        let saved = self.read().unwrap_or_else(|reason| {
            warn!(reason, "remote order unreadable");
            Vec::new()
        });
        let mut order: Vec<String> = Vec::with_capacity(registry.len());
        let known = saved.iter().filter(|name| registry.contains(name));
        let rest = registry.iter();
        for name in known.chain(rest) {
            if !order.contains(name) {
                order.push(name.clone());
            }
        }
        order
    }

    /// Writes `products` as given. `get` removes the names that are not in the registry.
    pub(crate) fn set(&self, products: &[String]) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let bytes = serde_json::to_vec_pretty(products)?;
        let temporary = self.file.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.file)?;
        info!(products = products.len(), "remote order saved");
        Ok(())
    }

    /// The names in the file, or the reason that the file cannot be used.
    fn read(&self) -> Result<Vec<String>, &'static str> {
        let text = std::fs::read_to_string(&self.file).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "missing"
            } else {
                "unreadable"
            }
        })?;
        serde_json::from_str(&text).map_err(|_| "malformed")
    }
}

#[cfg(test)]
#[path = "remote_order_tests.rs"]
mod tests;
