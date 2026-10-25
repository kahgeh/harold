//! The spec file check: whether the file a todo names still has the checksum the todo was
//! asked with. The tool opens a file only after resolving symlinks and finding it inside the
//! `.superpowers/pod/specs/` folder of a registered product (ADR 0015); it only hashes it.

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

    /// The path of the spec page of each spec file, `/specs/<product>/<id>`; empty for a file
    /// that is not directly in a product's specs folder, or whose name is not an id. The
    /// registry is read once for all.
    pub(super) fn pages(&self, files: &[String]) -> Vec<String> {
        let Ok(products) = self.products.products() else {
            return vec![String::new(); files.len()];
        };
        let folders: Vec<(String, PathBuf)> = products
            .into_iter()
            .filter(|(_, repo)| repo.is_absolute())
            .filter_map(|(name, repo)| {
                let folder = SPECS_FOLDER.iter().fold(repo, |p, part| p.join(part));
                Some((name, std::fs::canonicalize(folder).ok()?))
            })
            .collect();
        files
            .iter()
            .map(|file| page_of(&folders, file).unwrap_or_default())
            .collect()
    }

    /// The state of each todo, as `(spec_file, sha)`. The registry is read once for all.
    pub(super) fn states(&self, asked: &[(String, String)]) -> Vec<State> {
        let roots = self.roots();
        asked
            .iter()
            .map(|(file, sha)| self.state_in(&roots, file, sha))
            .collect()
    }

    /// The state of one todo asked with `sha`, whose spec is `spec_file`.
    #[cfg(test)]
    pub(super) fn state(&self, spec_file: &str, sha: &str) -> State {
        self.states(&[(spec_file.to_owned(), sha.to_owned())])[0]
    }

    fn state_in(&self, roots: &Result<Vec<PathBuf>, Reason>, spec_file: &str, sha: &str) -> State {
        // The `sha` is the first 12 or more hex digits of the checksum; anything else says
        // nothing about the file, so it is neither current nor changed.
        let Some(asked) = asked_checksum(sha).filter(|_| !spec_file.is_empty()) else {
            return State::Unknown(None);
        };
        match self.digest_of(roots, spec_file) {
            Err(reason) => State::Unknown(Some(reason)),
            Ok(digest) => match digest.starts_with(&asked) {
                true => State::Current,
                false => State::Changed,
            },
        }
    }

    /// The checksum of the file, in hex, read again only when its size or time changed.
    fn digest_of(
        &self,
        roots: &Result<Vec<PathBuf>, Reason>,
        spec_file: &str,
    ) -> Result<String, Reason> {
        let roots = roots.as_ref().map_err(|reason| *reason)?;
        // Resolved again at every call, so a symlink changed later cannot lead out.
        let resolved = std::fs::canonicalize(spec_file).map_err(|_| Reason::Missing)?;
        if !inside(&resolved, roots) {
            return Err(Reason::OutsideSpecs);
        }
        // Opened before it is judged, and judged by the open handle, so what is read is what
        // was checked. A pipe swapped in cannot block the open, and is not a regular file.
        let file = open_without_blocking(&resolved).map_err(|_| Reason::Missing)?;
        let metadata = file.metadata().map_err(|_| Reason::Missing)?;
        if !metadata.is_file() {
            return Err(Reason::NotAFile);
        }
        if metadata.len() > MAX_SPEC_BYTES {
            return Err(Reason::TooLarge);
        }
        let modified = metadata.modified().ok();
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(held) = cache.get(&resolved)
                && held.len == metadata.len()
                && held.modified == modified
            {
                return Ok(held.digest.clone());
            }
        }
        let digest = hash_of(file)?;
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
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

/// The spec page of `file`: its name is an id and it sits directly in one product's specs
/// folder, once symlinks are resolved. The page shows the file under that name.
fn page_of(folders: &[(String, PathBuf)], file: &str) -> Option<String> {
    if file.is_empty() {
        return None;
    }
    let path = Path::new(file);
    let id = path.file_name()?.to_str()?.strip_suffix(".md")?;
    let valid = !id.is_empty()
        && id.chars().count() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return None;
    }
    // The folder the name is in, and the file it resolves to, must both be in the folder.
    let folder = std::fs::canonicalize(path.parent()?).ok()?;
    let resolved = std::fs::canonicalize(path).ok()?;
    let (product, root) = folders.iter().find(|(_, root)| *root == folder)?;
    if !resolved.starts_with(root) {
        return None;
    }
    Some(format!("/specs/{}/{}", encode_segment(product), id))
}

/// Percent-encodes one path segment.
fn encode_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
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

/// `O_NONBLOCK`, so opening a pipe in a regular file's place returns at once.
const O_NONBLOCK: i32 = if cfg!(target_os = "macos") {
    0x0004
} else {
    0o4000
};

fn open_without_blocking(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

/// The checksum asked for: 12 to 64 hex digits, in lower case.
fn asked_checksum(sha: &str) -> Option<String> {
    let sha = sha.trim().to_ascii_lowercase();
    let hex = (12..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then_some(sha)
}

fn hash_of(file: File) -> Result<String, Reason> {
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
