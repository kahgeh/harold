//! The Hugging Face side of the TTS model downloads: `voice_download.py` run in a Python that
//! has `huggingface_hub` (ADR 0011): the `[voice_input]` Python on the Mac, `[tts] python` on
//! Linux. It runs with `-I`, a cleared environment, no token and a pinned commit, and checks
//! every file the commit lists. Harold keeps the record of the check.

use super::fetch::{DOWNLOAD_LIMIT, Env, folder_bytes, repo_dir, write_download_script};
use crate::tools::BoxFuture;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::{Instant, interval_at, timeout};

/// How often the progress of a download is read from the cache.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// How long the listing of a commit's files may take.
const LIST_LIMIT: Duration = Duration::from_secs(60);

/// What the download program may say went wrong; anything else is `failed`.
const REASONS: [&str; 5] = [
    "network",
    "not_found",
    "disk_full",
    "check_failed",
    "not_allowed",
];

/// Why a call to the Hub or the disk failed: a word from the download program.
pub(crate) type Reason = &'static str;

/// A file of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteFile {
    pub path: String,
    pub size: u64,
}

/// What the registry needs of the Hub. A test supplies a fake.
pub(crate) trait Fetcher: Send + Sync + 'static {
    /// The files of `repo` at `revision`, listed without downloading.
    fn list_files<'a>(
        &'a self,
        repo: &'a str,
        revision: &'a str,
    ) -> BoxFuture<'a, Result<Vec<RemoteFile>, Reason>>;
    /// Checks the cached files of `repo` at `revision` and writes `record`; `Err("missing")` when a
    /// file is not cached. Downloads nothing.
    fn check<'a>(
        &'a self,
        repo: &'a str,
        revision: &'a str,
        record: &'a Path,
    ) -> BoxFuture<'a, Result<u64, Reason>>;
    /// Downloads and checks `repo` at `revision`, writes `record`, and tells `progress` the bytes
    /// held and the total. Returns the bytes of the model.
    fn download<'a>(
        &'a self,
        repo: &'a str,
        revision: &'a str,
        record: &'a Path,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<u64, Reason>>;
}

/// The real Hub side: the download program in a child process.
pub(crate) struct ScriptFetcher {
    python: String,
    folder: PathBuf,
    env: Env,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Download,
    Check,
    List,
}

/// How a run of the download program ended.
struct Run {
    outcome: Result<u64, Reason>,
    files: Vec<RemoteFile>,
}

impl ScriptFetcher {
    pub(crate) fn new(python: String, folder: PathBuf, env: Env) -> Self {
        Self {
            python,
            folder,
            env,
        }
    }

    async fn run(
        &self,
        repo: &str,
        revision: &str,
        record: &Path,
        mode: Mode,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Run {
        let failed = |reason| Run {
            outcome: Err(reason),
            files: Vec::new(),
        };
        if write_download_script(&self.folder).is_err() {
            return failed("failed");
        }
        let flag = match mode {
            Mode::Download => None,
            Mode::Check => Some("--check"),
            Mode::List => Some("--list"),
        };
        let spawned = Command::new(&self.python)
            .arg("-I")
            .arg(self.folder.join("voice_download.py"))
            .arg(repo)
            .arg(revision)
            .arg(record)
            .args(flag)
            .env_clear()
            .envs(self.env.script_vars())
            .current_dir(&self.folder)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else {
            return failed("failed");
        };
        let Some(stdout) = child.stdout.take() else {
            return failed("failed");
        };
        let limit = if mode == Mode::List {
            LIST_LIMIT
        } else {
            DOWNLOAD_LIMIT
        };
        let run = timeout(limit, self.follow(repo, stdout, mode, progress)).await;
        let _ = child.kill().await;
        run.unwrap_or_else(|_| failed("failed"))
    }

    /// Reads the program's lines until it ends, and every `PROGRESS_EVERY` reports how many
    /// bytes the cache holds for the model.
    async fn follow(
        &self,
        repo: &str,
        stdout: tokio::process::ChildStdout,
        mode: Mode,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Run {
        let mut lines = BufReader::new(stdout).lines();
        let blobs = repo_dir(&self.env.cache(), repo).join("blobs");
        let mut total = 0;
        let mut files = Vec::new();
        let mut outcome = None;
        let mut ticker = interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let Ok(reply) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(listed) = reply["files"].as_array() {
                        files = listed
                            .iter()
                            .filter_map(|file| {
                                Some(RemoteFile {
                                    path: file["path"].as_str()?.to_string(),
                                    size: file["size"].as_u64()?,
                                })
                            })
                            .collect();
                    } else if let Some(size) = reply["total"].as_u64() {
                        total = size;
                        progress(0, total);
                    } else if reply["done"].as_bool() == Some(true) {
                        outcome = Some(Ok(reply["bytes"].as_u64().unwrap_or(total)));
                    } else if reply["done"].as_bool() == Some(false) {
                        let said = reply["reason"].as_str().unwrap_or_default();
                        let reason = if mode == Mode::Check && said == "missing" {
                            "missing"
                        } else {
                            REASONS
                                .iter()
                                .copied()
                                .find(|known| *known == said)
                                .unwrap_or("failed")
                        };
                        outcome = Some(Err(reason));
                    }
                }
                _ = ticker.tick(), if mode == Mode::Download => {
                    progress(folder_bytes(&blobs), total);
                }
            }
        }
        Run {
            outcome: outcome.unwrap_or(Err("failed")),
            files,
        }
    }
}

fn nothing(_: u64, _: u64) {}

impl Fetcher for ScriptFetcher {
    fn list_files<'a>(
        &'a self,
        repo: &'a str,
        revision: &'a str,
    ) -> BoxFuture<'a, Result<Vec<RemoteFile>, Reason>> {
        Box::pin(async move {
            let record = self.folder.join("unused.json");
            let run = self
                .run(repo, revision, &record, Mode::List, &nothing)
                .await;
            run.outcome.map(|_| run.files)
        })
    }

    fn check<'a>(
        &'a self,
        repo: &'a str,
        revision: &'a str,
        record: &'a Path,
    ) -> BoxFuture<'a, Result<u64, Reason>> {
        Box::pin(async move {
            self.run(repo, revision, record, Mode::Check, &nothing)
                .await
                .outcome
        })
    }

    fn download<'a>(
        &'a self,
        repo: &'a str,
        revision: &'a str,
        record: &'a Path,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<u64, Reason>> {
        Box::pin(async move {
            self.run(repo, revision, record, Mode::Download, progress)
                .await
                .outcome
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// A fake Python: an executable script that records its arguments and environment and
    /// answers as the test says. It ignores `-I` and the script.
    struct Fake {
        folder: PathBuf,
        python: String,
    }

    impl Fake {
        fn new(body: &str) -> Self {
            let folder = std::env::temp_dir().join(format!("harold-hub-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&folder).unwrap();
            let program = folder.join("python");
            std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                python: program.display().to_string(),
                folder,
            }
        }

        fn fetcher(&self) -> ScriptFetcher {
            ScriptFetcher::new(
                self.python.clone(),
                self.folder.join("tts-models"),
                Env {
                    path: "/usr/bin:/bin".into(),
                    home: self.folder.clone(),
                    hf_home: None,
                    hf_hub_cache: Some(self.folder.join("cache").display().to_string()),
                },
            )
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.folder);
        }
    }

    const REVISION: &str = "a71e4d38b236d968966a2002c4c895dbd12b1c3c";

    #[tokio::test]
    async fn it_lists_the_files_of_a_commit_without_downloading() {
        let fake = Fake::new(
            r#"echo "$@" > "$0.args"
printf '%s\n' '{"files": [{"path": "model.safetensors", "size": 10}, {"path": "config.json", "size": 2}]}'
printf '%s\n' '{"done": true, "bytes": 12}'"#,
        );

        let files = fake
            .fetcher()
            .list_files("org/name", REVISION)
            .await
            .unwrap();

        assert_eq!(
            files,
            [
                RemoteFile {
                    path: "model.safetensors".into(),
                    size: 10
                },
                RemoteFile {
                    path: "config.json".into(),
                    size: 2
                }
            ]
        );
        let args = std::fs::read_to_string(format!("{}.args", fake.python)).unwrap();
        assert!(args.contains("-I"));
        assert!(args.contains("org/name"));
        assert!(args.contains(REVISION));
        assert!(args.contains("--list"));
    }

    #[tokio::test]
    async fn it_downloads_and_reports_the_total_and_the_bytes() {
        let fake = Fake::new(
            r#"printf '%s\n' '{"total": 500}'
printf '%s\n' '{"done": true, "bytes": 500}'"#,
        );
        let seen = Mutex::new(Vec::new());
        let progress = |bytes: u64, total: u64| seen.lock().unwrap().push((bytes, total));
        let record = fake.folder.join("record.json");

        let bytes = fake
            .fetcher()
            .download("org/name", REVISION, &record, &progress)
            .await
            .unwrap();

        assert_eq!(bytes, 500);
        assert_eq!(*seen.lock().unwrap(), [(0, 500)]);
    }

    #[tokio::test]
    async fn a_check_that_finds_files_missing_says_missing_and_a_download_cannot() {
        let fake = Fake::new(r#"printf '%s\n' '{"done": false, "reason": "missing"}'"#);
        let record = fake.folder.join("record.json");

        assert_eq!(
            fake.fetcher().check("org/name", REVISION, &record).await,
            Err("missing")
        );
        assert_eq!(
            fake.fetcher()
                .download("org/name", REVISION, &record, &nothing)
                .await,
            Err("failed")
        );
    }

    #[tokio::test]
    async fn the_reasons_are_the_words_of_the_program_and_anything_else_is_failed() {
        for (said, reason) in [
            ("network", "network"),
            ("not_found", "not_found"),
            ("disk_full", "disk_full"),
            ("check_failed", "check_failed"),
            ("not_allowed", "not_allowed"),
            ("something else", "failed"),
        ] {
            let fake = Fake::new(&format!(
                r#"printf '%s\n' '{{"done": false, "reason": "{said}"}}'"#
            ));
            let result = fake
                .fetcher()
                .download("org/name", REVISION, &fake.folder.join("r.json"), &nothing)
                .await;
            assert_eq!(result, Err(reason), "{said}");
        }
    }

    #[tokio::test]
    async fn the_program_runs_with_a_cleared_environment_and_a_script_for_the_user_only() {
        let fake = Fake::new(
            r#"env > "$0.env"
printf '%s\n' '{"done": true, "bytes": 1}'"#,
        );
        fake.fetcher()
            .download("org/name", REVISION, &fake.folder.join("r.json"), &nothing)
            .await
            .unwrap();

        let env = std::fs::read_to_string(format!("{}.env", fake.python)).unwrap();
        assert!(env.contains("HF_HUB_CACHE="));
        assert!(!env.lines().any(|line| line.starts_with("USER=")));
        assert!(!env.contains("HF_TOKEN"));
        let script = fake.folder.join("tts-models/voice_download.py");
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn a_program_that_prints_nothing_fails() {
        let fake = Fake::new("exit 0");

        let result = fake
            .fetcher()
            .download("org/name", REVISION, &fake.folder.join("r.json"), &nothing)
            .await;

        assert_eq!(result, Err("failed"));
    }
}
