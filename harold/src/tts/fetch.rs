//! What the model downloads share: the Hugging Face cache layout, the machine's own answers
//! (free disk space, the default route), the hotspot rule, the pinned-commit checks, the
//! download program's file, and the one slot that lets a single download run at a time on the
//! machine (ADR 0011). The speech models (Whisper) and the TTS models both use it.

use crate::tools::BoxFuture;
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

/// The download program, written into a models folder before each run.
pub(crate) const VOICE_DOWNLOAD_PY: &str = include_str!("../voice_download.py");

/// Free space needed on top of a model's size before its download starts.
pub(crate) const DISK_MARGIN: u64 = 1_000_000_000;
/// A run of the download program that has not finished by now is stopped.
pub(crate) const DOWNLOAD_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);
/// How often a start-up download held back by a metered network looks again.
pub(crate) const METERED_RECHECK: Duration = Duration::from_secs(10 * 60);

/// The slot of the one download that runs at a time on this machine: a speech model and a TTS
/// model never download together. A download holds the guard until it ends.
pub(crate) fn download_slot() -> &'static tokio::sync::Mutex<()> {
    static SLOT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SLOT.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 40 lowercase hex characters.
pub(crate) fn is_revision(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `<org>/<name>` in letters, digits, `.`, `_` and `-`.
pub(crate) fn is_model_name(text: &str) -> bool {
    let word = |part: &str| {
        !part.is_empty()
            && !part.bytes().all(|byte| byte == b'.')
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    };
    text.split_once('/')
        .is_some_and(|(org, name)| word(org) && word(name))
}

/// What the download program and the cache are told about the machine.
#[derive(Debug, Clone)]
pub(crate) struct Env {
    pub path: String,
    pub home: PathBuf,
    pub hf_home: Option<String>,
    pub hf_hub_cache: Option<String>,
}

impl Env {
    pub(crate) fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self {
            path: var("PATH").unwrap_or_else(|| "/usr/bin:/bin".to_string()),
            home: var("HOME").map(PathBuf::from).unwrap_or_default(),
            hf_home: var("HF_HOME"),
            hf_hub_cache: var("HF_HUB_CACHE"),
        }
    }

    /// `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`.
    pub(crate) fn cache(&self) -> PathBuf {
        if let Some(cache) = &self.hf_hub_cache {
            return PathBuf::from(cache);
        }
        if let Some(home) = &self.hf_home {
            return Path::new(home).join("hub");
        }
        self.home.join(".cache/huggingface/hub")
    }

    /// The only variables the download program is given.
    pub(crate) fn script_vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            ("PATH", self.path.clone()),
            ("HOME", self.home.display().to_string()),
        ];
        vars.extend(self.hf_home.clone().map(|home| ("HF_HOME", home)));
        vars.extend(
            self.hf_hub_cache
                .clone()
                .map(|cache| ("HF_HUB_CACHE", cache)),
        );
        vars
    }
}

/// `<cache>/models--<org>--<name>`.
pub(crate) fn repo_dir(cache: &Path, model: &str) -> PathBuf {
    cache.join(format!("models--{}", model.replace('/', "--")))
}

/// The folder of `model` at `commit`: `<cache>/models--<org>--<name>/snapshots/<commit>`.
pub(crate) fn snapshot_dir(cache: &Path, model: &str, commit: &str) -> PathBuf {
    repo_dir(cache, model).join("snapshots").join(commit)
}

/// What the machine says about itself: free disk space and the default route's gateway.
pub(crate) trait Host: Send + Sync + 'static {
    /// The bytes free on the disk holding `dir`.
    fn free_bytes<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, Option<u64>>;
    /// The default route's gateway.
    fn gateway(&self) -> BoxFuture<'_, Option<Ipv4Addr>>;
}

/// The machine's own answers, from `df` and `route` (the Mac) or `ip` (Linux).
pub(crate) struct SystemHost;

impl Host for SystemHost {
    fn free_bytes<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, Option<u64>> {
        Box::pin(async move {
            let mut existing = dir;
            while !existing.exists() {
                existing = existing.parent()?;
            }
            let output = Command::new("/bin/df")
                .arg("-Pk")
                .arg(existing)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .ok()?;
            let text = String::from_utf8_lossy(&output.stdout);
            let available: u64 = text
                .lines()
                .nth(1)?
                .split_whitespace()
                .nth(3)?
                .parse()
                .ok()?;
            Some(available.saturating_mul(1024))
        })
    }

    fn gateway(&self) -> BoxFuture<'_, Option<Ipv4Addr>> {
        Box::pin(async move {
            #[cfg(target_os = "linux")]
            {
                let output = Command::new("ip")
                    .args(["-4", "route", "show", "default"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .await
                    .ok()?;
                let text = String::from_utf8_lossy(&output.stdout);
                let mut words = text.split_whitespace();
                words.find(|word| *word == "via")?;
                words.next()?.parse().ok()
            }
            #[cfg(not(target_os = "linux"))]
            {
                let output = Command::new("/sbin/route")
                    .args(["-n", "get", "default"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .await
                    .ok()?;
                let text = String::from_utf8_lossy(&output.stdout);
                text.lines()
                    .find_map(|line| line.trim().strip_prefix("gateway:"))
                    .and_then(|gateway| gateway.trim().parse().ok())
            }
        })
    }
}

/// The iPhone Personal Hotspot's own range, `172.20.10.0/28`.
pub(crate) fn is_hotspot(gateway: Ipv4Addr) -> bool {
    let [a, b, c, d] = gateway.octets();
    (a, b, c) == (172, 20, 10) && d < 16
}

/// The bytes of the files in `dir`, partial downloads included.
pub(crate) fn folder_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Writes the download program into `folder` (made 0700), as a file for the user only, never
/// reusing one that is there.
pub(crate) fn write_download_script(folder: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)?;
    std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))?;
    let script = folder.join("voice_download.py");
    match std::fs::remove_file(&script) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&script)?;
    std::io::Write::write_all(&mut file, VOICE_DOWNLOAD_PY.as_bytes())
}
