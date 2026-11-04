//! Pairing a slave with its master. The master keeps one secret for each slave in
//! `slaves/<name>.secret`; the slave keeps its secret in `master.secret`. Both files have
//! mode 0600. A secret leaves this module only as the text `harold slaves pair` prints once.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The length of a secret as hex text: 32 bytes.
const SECRET_HEX_LEN: usize = 64;

/// A `harold slaves` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlavesCommand {
    Pair { name: String },
    Unpair { name: String },
}

/// A `harold master` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MasterCommand {
    Pair { secret: String, address: String },
}

/// A new secret: 32 random bytes as 64 hex characters, from two random UUIDs.
pub(crate) fn new_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Whether two secrets are equal. The time does not depend on where they differ.
pub(crate) fn secrets_equal(a: &str, b: &str) -> bool {
    let a = Sha256::digest(a.as_bytes());
    let b = Sha256::digest(b.as_bytes());
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

/// Where the master keeps the secret of the slave `name`.
pub(crate) fn master_secret_path(data: &Path, name: &str) -> PathBuf {
    data.join("slaves").join(format!("{name}.secret"))
}

/// Where the slave keeps the secret of its master.
pub(crate) fn slave_secret_path(data: &Path) -> PathBuf {
    data.join("master.secret")
}

/// Writes `secret` to `path` with mode 0600. The file is whole or absent: it is written to a
/// temporary file first and renamed.
pub(crate) fn write_secret(path: &Path, secret: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("secret.tmp");
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(secret.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

/// The secret in `path`, or `None` when the file is missing, unreadable or empty.
pub(crate) fn read_secret(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let secret = text.trim();
    (!secret.is_empty()).then(|| secret.to_owned())
}

/// A name that is safe to use in a file name: a host label that does not start with a dot.
fn safe_name(name: &str) -> bool {
    crate::agent::pane_route::valid_host_label(name) && !name.starts_with('.')
}

/// Makes and keeps the secret of the slave `name`, and returns it. `names` is `[slaves] names`.
pub(crate) fn pair_slave(data: &Path, names: &[String], name: &str) -> Result<String, String> {
    if !safe_name(name) {
        return Err(format!("{name} is not a valid slave name"));
    }
    if !names.iter().any(|listed| listed == name) {
        return Err(format!("{name} is not in [slaves] names"));
    }
    let secret = new_secret();
    write_secret(&master_secret_path(data, name), &secret)
        .map_err(|_| format!("the secret of {name} cannot be written"))?;
    Ok(secret)
}

/// Removes the secret of the slave `name`. A missing secret is not an error.
pub(crate) fn unpair_slave(data: &Path, name: &str) -> Result<(), String> {
    if !safe_name(name) {
        return Err(format!("{name} is not a valid slave name"));
    }
    match std::fs::remove_file(master_secret_path(data, name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(format!("the secret of {name} cannot be removed")),
    }
}

/// Runs a `harold slaves` command. `Ok` is the text to print. Only `pair` prints a secret.
pub(crate) fn run_slaves(
    data: &Path,
    names: &[String],
    command: &SlavesCommand,
) -> Result<String, String> {
    match command {
        SlavesCommand::Pair { name } => {
            let secret = pair_slave(data, names, name)?;
            Ok(format!(
                "Paired {name}. The secret is shown once. Run on {name}: harold master pair {secret} --address <master address and port>"
            ))
        }
        SlavesCommand::Unpair { name } => {
            unpair_slave(data, name)?;
            Ok(format!("Unpaired {name}. Its next connect is refused."))
        }
    }
}

/// Whether `address` is `<host>:<port>`: a host of safe characters and a port number.
pub(crate) fn valid_address(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '[' | ']' | ':'))
        && port.parse::<u16>().is_ok_and(|port| port > 0)
}

/// `local.toml` text with its `[master]` table replaced by one with `address`. Other lines
/// stay as they are.
fn with_master_address(text: &str, address: &str) -> String {
    let mut kept = String::new();
    let mut in_master = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_master = trimmed == "[master]";
        }
        if !in_master {
            kept.push_str(line);
        }
    }
    let kept = kept.trim_end_matches('\n');
    let table = format!("[master]\naddress = \"{address}\"\n");
    if kept.is_empty() {
        table
    } else {
        format!("{kept}\n\n{table}")
    }
}

/// Runs `harold master pair`: keeps the secret and writes `[master] address` in `local`.
/// `Ok` is the text to print, which holds no secret.
pub(crate) fn run_master(
    data: &Path,
    local: &Path,
    secret: &str,
    address: &str,
) -> Result<String, String> {
    if secret.len() != SECRET_HEX_LEN || !secret.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("the secret must be 64 hex characters".into());
    }
    if !valid_address(address) {
        return Err(format!("the address must be <host>:<port>: {address}"));
    }
    let text = match std::fs::read_to_string(local) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err(format!("{} cannot be read", local.display())),
    };
    let updated = with_master_address(&text, address);
    write_secret(&slave_secret_path(data), secret)
        .map_err(|_| "the secret cannot be written".to_string())?;
    let temporary = local.with_extension("toml.tmp");
    std::fs::write(&temporary, updated)
        .and_then(|()| match std::fs::metadata(local) {
            Ok(old) => std::fs::set_permissions(&temporary, old.permissions()),
            Err(_) => Ok(()),
        })
        .and_then(|()| std::fs::rename(&temporary, local))
        .map_err(|_| format!("{} cannot be written", local.display()))?;
    Ok(format!("Paired with the master at {address}."))
}

#[cfg(test)]
#[path = "pairing_tests.rs"]
mod tests;
