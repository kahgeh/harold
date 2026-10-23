//! The two secret files: who may read them and where they may lie.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Why a secret file was refused. It is logged with the file's path, never its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Unreadable,
    ReadableByOthers,
    InsideVault,
    InsideGitCheckout,
}

impl Refusal {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Refusal::Unreadable => "unreadable",
            Refusal::ReadableByOthers => "readable_by_others",
            Refusal::InsideVault => "inside_vault",
            Refusal::InsideGitCheckout => "inside_git_checkout",
        }
    }
}

/// Checks a secret file: it exists, no one but its owner can read it, and it lies outside
/// the vault and outside every git checkout.
pub(crate) fn check_secret_file(file: &Path, vault: &Path) -> Result<(), Refusal> {
    let metadata = std::fs::metadata(file).map_err(|_| Refusal::Unreadable)?;
    if !metadata.is_file() {
        return Err(Refusal::Unreadable);
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(Refusal::ReadableByOthers);
    }
    let real = file.canonicalize().map_err(|_| Refusal::Unreadable)?;
    if let Ok(vault) = vault.canonicalize()
        && real.starts_with(&vault)
    {
        return Err(Refusal::InsideVault);
    }
    if real
        .ancestors()
        .skip(1)
        .any(|dir| dir.join(".git").exists())
    {
        return Err(Refusal::InsideGitCheckout);
    }
    Ok(())
}

/// The `access_key` and `secret_key` lines of the credentials file.
pub(crate) fn read_credentials(file: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(file).ok()?;
    let value = |key: &str| {
        content.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key && !value.trim().is_empty()).then(|| value.trim().to_string())
        })
    };
    Some((value("access_key")?, value("secret_key")?))
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
