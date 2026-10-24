//! The spec file check: whether the file a todo names still has the checksum the todo was
//! asked with. The tool opens a file only after resolving symlinks and finding it inside the
//! `.superpowers/pod/specs/` folder of a registered product (ADR 0014); it only hashes it.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::tools::Products;

/// The folder inside a product's repo that holds its specs.
const SPECS_FOLDER: [&str; 3] = [".superpowers", "pod", "specs"];
/// A spec file larger than this is not read.
const MAX_SPEC_BYTES: u64 = 1024 * 1024;

/// Why a spec's state is `unknown`, as `todo spec unknown` logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Reason {
    Missing,
    NotAFile,
    TooLarge,
    OutsideSpecs,
    RegistryUnreadable,
}

impl Reason {
    pub(super) fn word(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotAFile => "not_a_file",
            Self::TooLarge => "too_large",
            Self::OutsideSpecs => "outside_specs",
            Self::RegistryUnreadable => "registry_unreadable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Current,
    Changed,
    /// `None` when the todo names no spec file, which is not worth a log line.
    Unknown(Option<Reason>),
}

/// What was last read of a spec file: read again only when its size or time changes.
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    digest: String,
}

pub(super) struct SpecChecker {
    products: Arc<dyn Products>,
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

impl SpecChecker {
    pub(super) fn new(products: Arc<dyn Products>) -> Self {
        Self {
            products,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `spec_file` may be stored: once symlinks are resolved it sits inside a
    /// registered product's specs folder. A file that does not exist yet passes when its
    /// folder resolves to one. `Err(RegistryUnreadable)` is the caller's to accept.
    pub(super) fn may_put(&self, spec_file: &str) -> Result<(), Reason> {
        let roots = self.roots()?;
        let resolved = resolve_for_put(Path::new(spec_file)).ok_or(Reason::OutsideSpecs)?;
        match inside(&resolved, &roots) {
            true => Ok(()),
            false => Err(Reason::OutsideSpecs),
        }
    }

    /// The state of the todo asked with `sha`, whose spec is `spec_file`.
    pub(super) fn state(&self, spec_file: &str, sha: &str) -> State {
        if spec_file.is_empty() || sha.trim().is_empty() {
            return State::Unknown(None);
        }
        match self.digest_of(spec_file) {
            Err(reason) => State::Unknown(Some(reason)),
            Ok(digest) => match digest.starts_with(&sha.trim().to_ascii_lowercase()) {
                true => State::Current,
                false => State::Changed,
            },
        }
    }

    /// The checksum of the file, in hex, read again only when its size or time changed.
    fn digest_of(&self, spec_file: &str) -> Result<String, Reason> {
        let roots = self.roots()?;
        // Resolved again at every call, so a symlink changed later cannot lead out.
        let resolved = std::fs::canonicalize(spec_file).map_err(|_| Reason::Missing)?;
        if !inside(&resolved, &roots) {
            return Err(Reason::OutsideSpecs);
        }
        let metadata = std::fs::metadata(&resolved).map_err(|_| Reason::Missing)?;
        if !metadata.is_file() {
            return Err(Reason::NotAFile);
        }
        if metadata.len() > MAX_SPEC_BYTES {
            return Err(Reason::TooLarge);
        }
        let modified = metadata.modified().ok();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(held) = cache.get(&resolved)
            && held.len == metadata.len()
            && held.modified == modified
        {
            return Ok(held.digest.clone());
        }
        let digest = hash_of(&resolved)?;
        cache.insert(
            resolved,
            Cached {
                len: metadata.len(),
                modified,
                digest: digest.clone(),
            },
        );
        Ok(digest)
    }

    /// The resolved specs folders of the registered products that have one.
    fn roots(&self) -> Result<Vec<PathBuf>, Reason> {
        let products = self
            .products
            .products()
            .map_err(|_| Reason::RegistryUnreadable)?;
        Ok(products
            .into_iter()
            .filter(|(_, repo)| repo.is_absolute())
            .filter_map(|(_, repo)| {
                std::fs::canonicalize(SPECS_FOLDER.iter().fold(repo, |p, part| p.join(part))).ok()
            })
            .collect())
    }
}

fn inside(resolved: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| resolved.starts_with(root))
}

/// The path with symlinks resolved. A file that is not there yet is its folder, resolved,
/// and its name; a name that is a dangling symlink is not accepted.
fn resolve_for_put(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return None;
    }
    let name = path.file_name()?;
    let folder = std::fs::canonicalize(path.parent()?).ok()?;
    Some(folder.join(name))
}

fn hash_of(path: &Path) -> Result<String, Reason> {
    let file = File::open(path).map_err(|_| Reason::Missing)?;
    let mut bytes = Vec::new();
    // One byte past the limit, so a file that grew since the stat is caught.
    file.take(MAX_SPEC_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Reason::NotAFile)?;
    if bytes.len() as u64 > MAX_SPEC_BYTES {
        return Err(Reason::TooLarge);
    }
    Ok(hex::encode(Sha256::digest(&bytes)))
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
