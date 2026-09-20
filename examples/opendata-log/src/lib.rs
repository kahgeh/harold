//! Small shared helpers; each lesson keeps its stream behavior in its example.
use bytes::Bytes;
use opendata_common::storage::config::{
    AwsObjectStoreConfig, LocalObjectStoreConfig, SlateDbStorageConfig,
};
use opendata_common::{ObjectStoreConfig, StorageConfig};
use opendata_log::{Config, LogDb, LogRead, ReaderConfig, Record};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn required(name: &str) -> Result<String> {
    let value =
        env::var(name).map_err(|_| format!("Set {name} explicitly before running this lesson"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(value)
}

fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty()
        || prefix
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("Use a nonempty isolated OPENDATA_PREFIX, without leading/trailing slash or dot segments".into());
    }
    Ok(())
}

pub fn backend() -> Result<String> {
    match env::var("OPENDATA_BACKEND") {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok("local".into()),
        Err(error) => Err(error.into()),
    }
}

pub fn local_config(directory: &Path, prefix: &str) -> Result<Config> {
    validate_prefix(prefix)?;
    fs::create_dir_all(directory)?;
    let directory = fs::canonicalize(directory)?;
    Ok(storage_config(
        prefix,
        ObjectStoreConfig::Local(LocalObjectStoreConfig {
            path: directory
                .to_str()
                .ok_or("local directory must be UTF-8")?
                .into(),
        }),
    ))
}

fn storage_config(prefix: &str, object_store: ObjectStoreConfig) -> Config {
    Config {
        storage: StorageConfig::SlateDb(SlateDbStorageConfig {
            path: prefix.into(),
            object_store,
            settings_path: None,
            block_cache: None,
            meta_cache: None,
        }),
        ..Config::default()
    }
}

/// Cloud runs use an existing, explicitly selected bucket and isolated prefix.
/// No credentials are printed, and no environment variables are changed.
pub fn config(namespace: &str) -> Result<Config> {
    // Keep the experiment's WAL/durability settings reproducible.
    if env::vars_os().any(|(name, _)| name.to_string_lossy().starts_with("SLATEDB_"))
        || [
            "SlateDb.toml",
            "SlateDb.json",
            "SlateDb.yaml",
            "SlateDb.yml",
        ]
        .iter()
        .any(|path| Path::new(path).exists())
    {
        return Err("Remove inherited SLATEDB_* variables and SlateDb settings files before running these controlled lessons".into());
    }
    let backend = backend()?;
    if backend == "memory" {
        return Ok(Config {
            storage: StorageConfig::InMemory,
            ..Config::default()
        });
    }
    if backend == "local" {
        let prefix = env::var("OPENDATA_PREFIX").unwrap_or_else(|_| namespace.into());
        let directory = env::var_os("OPENDATA_LOCAL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".data"));
        return local_config(&directory, &prefix);
    }
    if backend != "s3" && backend != "express" {
        return Err("OPENDATA_BACKEND must be memory, local, s3, or express".into());
    }
    // This tutorial's audit covers genuine AWS endpoints and the default conditional-put fencing.
    for name in [
        "AWS_ENDPOINT",
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT_URL_STS",
        "AWS_ALLOW_HTTP",
        "AWS_ALLOW_INVALID_CERTIFICATES",
        "AWS_CONDITIONAL_PUT",
    ] {
        if env::var_os(name).is_some() {
            return Err(format!("Unset {name}: this lesson requires default genuine AWS endpoint/TLS/conditional-put behavior").into());
        }
    }
    let express = env::var("AWS_S3_EXPRESS").unwrap_or_default();
    if (backend == "express" && express != "true")
        || (backend == "s3" && !express.is_empty() && express != "false")
    {
        return Err(
            "Express requires AWS_S3_EXPRESS=true; S3 Standard requires it unset or false".into(),
        );
    }
    let prefix = required("OPENDATA_PREFIX")?;
    validate_prefix(&prefix)?;
    let region = required("AWS_REGION")?;
    let bucket = required("AWS_BUCKET")?;
    if bucket.ends_with("--x-s3") != (backend == "express") {
        return Err(
            "Use a directory bucket ending --x-s3 only with OPENDATA_BACKEND=express".into(),
        );
    }
    Ok(storage_config(
        &prefix,
        ObjectStoreConfig::Aws(AwsObjectStoreConfig { region, bucket }),
    ))
}

pub fn reader_config(config: &Config) -> ReaderConfig {
    ReaderConfig {
        storage: config.storage.clone(),
        refresh_interval: Duration::from_millis(100),
    }
}

pub fn checkpoint_path(namespace: &str) -> PathBuf {
    env::var_os("OPENDATA_CHECKPOINT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".checkpoints").join(format!("{namespace}.json")))
}

/// Application-owned monitor state. Only one process may write a checkpoint file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Checkpoints {
    #[serde(default)]
    pub source: Option<String>,
    pub next: BTreeMap<String, u64>,
}

impl Checkpoints {
    pub fn position(&self, key: &str) -> u64 {
        self.next.get(key).copied().unwrap_or(0)
    }

    pub fn bind_source(&mut self, config: &Config) -> Result<()> {
        let source = serde_json::to_string(&config.storage)?;
        match &self.source {
            Some(existing) if existing != &source => {
                return Err(
                    "Checkpoint belongs to a different database; choose a new OPENDATA_CHECKPOINT"
                        .into(),
                );
            }
            _ => self.source = Some(source),
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Write + sync a temporary sibling, rename, then sync the directory.
    /// This avoids torn JSON; it cannot atomically commit an external side effect.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let name = path
            .file_name()
            .ok_or("checkpoint needs a file name")?
            .to_string_lossy();
        let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| -> Result<()> {
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// Lessons 4, 5, and 8 run exactly the same experiment on different storage.
pub async fn durable_round_trip(config: Config, checkpoint_path: &Path) -> Result<()> {
    if matches!(config.storage, StorageConfig::InMemory) {
        return Err("This lesson requires persistent storage".into());
    }
    let mut checkpoints = Checkpoints::load(checkpoint_path)?;
    checkpoints.bind_source(&config)?;
    let log = LogDb::open(config.clone()).await?;
    let appended = log
        .try_append(vec![
            Record {
                key: Bytes::from_static(b"agent-a"),
                value: Bytes::from_static(b"Progress: inspected 2 files"),
            },
            Record {
                key: Bytes::from_static(b"agent-b"),
                value: Bytes::from_static(b"Progress: ran 3 checks"),
            },
        ])
        .await?;
    log.flush().await?;
    println!(
        "Durable batch starts at {}; closing writer.",
        appended.start_sequence
    );
    log.close().await?;

    let log = LogDb::open(config.clone()).await?;
    for (index, key) in ["agent-a", "agent-b"].iter().enumerate() {
        let before = checkpoints.position(key);
        let expected = appended.start_sequence + index as u64;
        if before > expected {
            return Err("Checkpoint is ahead of this database (was its storage reset?)".into());
        }
        let mut records = log
            .scan(Bytes::from_static(key.as_bytes()), before..)
            .await?;
        let mut found_new = false;
        while let Some(record) = records.next().await? {
            println!(
                "  replay [{}] {key}: {}",
                record.sequence,
                std::str::from_utf8(&record.value)?
            );
            found_new |= record.sequence == expected;
            // Advance only after successfully handling this durable record.
            checkpoints.next.insert(
                (*key).into(),
                record.sequence.checked_add(1).ok_or("sequence overflow")?,
            );
        }
        assert!(
            found_new,
            "durably acknowledged new record must be recovered"
        );
    }
    checkpoints.save(checkpoint_path)?;
    log.close().await?;

    let saved = Checkpoints::load(checkpoint_path)?;
    let log = LogDb::open(config).await?;
    for key in ["agent-a", "agent-b"] {
        assert!(
            log.scan(Bytes::from_static(key.as_bytes()), saved.position(key)..)
                .await?
                .next()
                .await?
                .is_none()
        );
    }
    log.close().await?;
    println!(
        "Saved checkpoints at {}; another reopen has no unread reports.",
        checkpoint_path.display()
    );
    Ok(())
}
