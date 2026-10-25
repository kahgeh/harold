//! Kokoro for Linux comes as two files of the GitHub release `model-files-v1.0` of
//! `thewh1teagle/kokoro-onnx`. The release publishes no SHA-256, so Harold pins each file's size
//! and SHA-256 in this code and keeps a file only when both match. The user approved this one
//! source (ADR 0020, Choice E (a)); it is not Hugging Face and `voices-v1.0.bin` is a `.bin`
//! file.
//!
//! The download asks only for the two fixed URLs, follows redirects only to GitHub's release
//! download hosts, and sends no token, no cookie and no `Authorization` header. It writes each
//! file to a temporary file, computes the SHA-256 while it writes, and renames the file into
//! place only when the SHA-256 equals the pin. A file that differs is deleted.

use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::warn;

/// The repository, as the model's id.
pub(crate) const MODEL_ID: &str = "thewh1teagle/kokoro-onnx";
/// The release tag, as the model's revision.
pub(crate) const RELEASE_TAG: &str = "model-files-v1.0";
const RELEASE_BASE: &str =
    "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0";
/// The hosts a redirect may lead to: where GitHub serves release files from.
const REDIRECT_HOSTS: [&str; 2] = [
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];
/// The most redirects one download follows.
const MAX_REDIRECTS: usize = 5;
/// How long the download waits for a connection, and for the next piece of the body.
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
const READ_LIMIT: Duration = Duration::from_secs(60);

/// A file of the release: its name, size in bytes and SHA-256 in lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// What a pin holds before the user records the file's SHA-256. No file has it, so a download
/// is refused with `check_failed` and nothing is kept. The user records the pins from one
/// download on the machine (docs/references/tts/README.md).
pub(crate) const PIN_NOT_RECORDED: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The two files of the release, with the sizes the release lists. The SHA-256 of each is
/// recorded from one download by the user (see `PIN_NOT_RECORDED`).
pub(crate) const FILES: [ReleaseFile; 2] = [
    ReleaseFile {
        name: "kokoro-v1.0.onnx",
        size: 325_532_387,
        sha256: PIN_NOT_RECORDED,
    },
    ReleaseFile {
        name: "voices-v1.0.bin",
        size: 28_214_398,
        sha256: PIN_NOT_RECORDED,
    },
];

/// The model file and the voices file's names in the model's folder.
pub(crate) const MODEL_FILE: &str = FILES[0].name;
pub(crate) const VOICES_FILE: &str = FILES[1].name;

/// The size of both files together.
pub(crate) fn total_size() -> u64 {
    FILES.iter().map(|file| file.size).sum()
}

/// Downloads the release files.
pub(crate) struct ReleaseDownloader {
    base: String,
    files: Vec<ReleaseFile>,
    client: reqwest::Client,
}

/// Why a download failed, as `tts model download failed` names it, with `disk_full` and
/// `failed` for the two other ways.
pub(crate) type Reason = &'static str;

impl ReleaseDownloader {
    /// The download of the real release.
    pub(crate) fn production() -> Self {
        Self::build(
            RELEASE_BASE.to_string(),
            FILES.to_vec(),
            REDIRECT_HOSTS.iter().map(|host| host.to_string()).collect(),
            false,
        )
    }

    /// A download of `files` from `base`, following redirects to `hosts` (over plain HTTP as
    /// well), for a test against a fake server.
    #[cfg(test)]
    pub(crate) fn for_test(base: String, files: Vec<ReleaseFile>, hosts: Vec<String>) -> Self {
        Self::build(base, files, hosts, true)
    }

    fn build(base: String, files: Vec<ReleaseFile>, hosts: Vec<String>, plain_http: bool) -> Self {
        let policy = Policy::custom(move |attempt| {
            let allowed = attempt.url().host_str().is_some_and(|host| {
                hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host))
            }) && (plain_http || attempt.url().scheme() == "https");
            if attempt.previous().len() > MAX_REDIRECTS || !allowed {
                // The redirect itself comes back as the answer, and is refused.
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .redirect(policy)
            .referer(false)
            .connect_timeout(CONNECT_LIMIT)
            .read_timeout(READ_LIMIT)
            .no_proxy()
            .build()
            .expect("an HTTP client with no extra settings");
        Self {
            base,
            files,
            client,
        }
    }

    /// Downloads each file into `folder` (made 0700), checks it
    /// and writes `record` once all are in place. `progress` is told the bytes of this run so
    /// far. Returns the bytes downloaded.
    pub(crate) async fn download_all(
        &self,
        folder: &Path,
        record: &Path,
        progress: impl Fn(u64),
    ) -> Result<u64, Reason> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)
            .map_err(|_| "failed")?;
        let mut bytes = 0;
        let mut checked = Vec::new();
        for file in &self.files {
            if file.sha256 == PIN_NOT_RECORDED {
                warn!(
                    model = MODEL_ID,
                    "tts model pin not recorded: the SHA-256 of a release file is not in the code"
                );
                return Err("check_failed");
            }
            let size = self.fetch(file, folder, bytes, &progress).await?;
            bytes += size;
            checked.push(file);
        }
        write_record(record, &checked)?;
        Ok(bytes)
    }

    /// Downloads one file to `<name>.part`, and renames it to `<name>` when its size and
    /// SHA-256 are the pinned ones. A file that differs is deleted.
    async fn fetch(
        &self,
        file: &ReleaseFile,
        folder: &Path,
        before: u64,
        progress: &impl Fn(u64),
    ) -> Result<u64, Reason> {
        let url = format!("{}/{}", self.base, file.name);
        let mut response = self.client.get(&url).send().await.map_err(|_| "network")?;
        if response.status().is_redirection() {
            // A redirect to a host that is not GitHub's: refused.
            return Err("check_failed");
        }
        if !response.status().is_success() {
            return Err("network");
        }
        let part = folder.join(format!("{}.part", file.name));
        let _ = std::fs::remove_file(&part);
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&part)
            .map_err(|_| "failed")?;
        let outcome = self
            .write_body(&mut response, &mut out, file, before, progress)
            .await;
        drop(out);
        let digest = match outcome {
            Ok(digest) => digest,
            Err(reason) => {
                let _ = std::fs::remove_file(&part);
                return Err(reason);
            }
        };
        if digest != file.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err("check_failed");
        }
        std::fs::rename(&part, folder.join(file.name)).map_err(|_| "failed")?;
        Ok(file.size)
    }

    /// Writes the body to `out`, and returns its SHA-256 when it is exactly `file.size` bytes.
    async fn write_body(
        &self,
        response: &mut reqwest::Response,
        out: &mut std::fs::File,
        file: &ReleaseFile,
        before: u64,
        progress: &impl Fn(u64),
    ) -> Result<String, Reason> {
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        while let Some(piece) = response.chunk().await.map_err(|_| "network")? {
            written += piece.len() as u64;
            if written > file.size {
                return Err("check_failed");
            }
            hasher.update(&piece);
            out.write_all(&piece).map_err(disk_reason)?;
            progress(before + written);
        }
        if written != file.size {
            return Err("check_failed");
        }
        Ok(hex::encode(hasher.finalize()))
    }
}

fn disk_reason(error: std::io::Error) -> Reason {
    if error.raw_os_error() == Some(28) {
        "disk_full"
    } else {
        "failed"
    }
}

/// The record of the check, as the Hugging Face path writes it: the files, with their sizes and
/// SHA-256, and the commit (here the release tag).
fn write_record(record: &Path, files: &[&ReleaseFile]) -> Result<(), Reason> {
    let body = serde_json::json!({
        "model": MODEL_ID,
        "commit": RELEASE_TAG,
        "files": files
            .iter()
            .map(|file| serde_json::json!({
                "path": file.name,
                "size": file.size,
                "sha256": file.sha256,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string();
    let temporary = PathBuf::from(format!("{}.tmp", record.display()));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| "failed")?;
    out.write_all(body.as_bytes()).map_err(|_| "failed")?;
    drop(out);
    std::fs::rename(&temporary, record).map_err(|_| "failed")
}

#[cfg(test)]
#[path = "release_download_tests.rs"]
mod tests;
