//! The approved model list, `tts-models.toml` beside `local.toml`: the TTS models Harold may
//! download, each written after an audit. Only a person edits it. Harold reads it each time a
//! model is asked for, so an entry takes effect without a restart, and ignores a file that
//! other users can write.
//!
//! ```toml
//! [[model]]
//! source = "huggingface"
//! repo = "mlx-community/Kokoro-82M-bf16"
//! revision = "a71e4d38b236d968966a2002c4c895dbd12b1c3c"
//! worker = "mlx"
//! licence = "Apache-2.0"
//! audit = "docs/references/dependency-audits.md: TTS model rows"
//! ```

use super::fetch::{is_model_name, is_revision};
use super::worker::WorkerKind;
use config::{Config, File, FileFormat};
use serde::Deserialize;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tracing::warn;

/// One entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Approved {
    pub repo: String,
    pub revision: String,
    pub worker: WorkerKind,
    pub licence: String,
    pub audit: String,
}

#[derive(Debug, Deserialize)]
struct Raw {
    source: String,
    repo: String,
    revision: String,
    worker: String,
    licence: String,
    audit: String,
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(default)]
    model: Vec<Raw>,
}

/// Reads the list at `path`. A missing file is an empty list. A file that others can write, or
/// that does not parse, is ignored (empty) and logged once with its reason; an entry that is
/// refused is logged and left out. No log line holds the file's text.
pub(crate) fn read(path: &Path) -> Vec<Approved> {
    let text = match std::fs::metadata(path) {
        Err(_) => return Vec::new(),
        Ok(meta) if meta.permissions().mode() & 0o022 != 0 => {
            warn!(reason = "mode", "tts approved list ignored");
            return Vec::new();
        }
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => {
                warn!(reason = "unreadable", "tts approved list ignored");
                return Vec::new();
            }
        },
    };
    parse(&text)
}

pub(crate) fn parse(text: &str) -> Vec<Approved> {
    let document = Config::builder()
        .add_source(File::from_str(text, FileFormat::Toml))
        .build()
        .and_then(Config::try_deserialize::<Document>);
    let Ok(document) = document else {
        warn!(reason = "invalid", "tts approved list ignored");
        return Vec::new();
    };
    document
        .model
        .into_iter()
        .filter_map(|raw| {
            let entry = entry_of(&raw);
            if entry.is_none() {
                warn!(reason = "entry_refused", "tts approved list entry left out");
            }
            entry
        })
        .collect()
}

fn entry_of(raw: &Raw) -> Option<Approved> {
    // Another source needs its own code and the user's approval in its entry: only
    // Hugging Face is read here.
    if raw.source != "huggingface"
        || !is_model_name(&raw.repo)
        || !is_revision(&raw.revision)
        || raw.licence.trim().is_empty()
        || raw.audit.trim().is_empty()
    {
        return None;
    }
    Some(Approved {
        repo: raw.repo.clone(),
        revision: raw.revision.clone(),
        worker: WorkerKind::from_word(&raw.worker)?,
        licence: raw.licence.trim().to_string(),
        audit: raw.audit.trim().to_string(),
    })
}

/// The entry for `repo` at `revision`.
pub(crate) fn find<'a>(list: &'a [Approved], repo: &str, revision: &str) -> Option<&'a Approved> {
    list.iter()
        .find(|entry| entry.repo == repo && entry.revision == revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "a71e4d38b236d968966a2002c4c895dbd12b1c3c";

    fn entry(extra: &str) -> String {
        format!(
            "[[model]]\nsource = \"huggingface\"\nrepo = \"org/name\"\nrevision = \"{REVISION}\"\nworker = \"mlx\"\nlicence = \"MIT\"\naudit = \"row 3\"\n{extra}"
        )
    }

    fn scratch(mode: u32, text: &str) -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("harold-approved-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("tts-models.toml");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn an_entry_is_read_with_its_worker_licence_and_audit() {
        let list = parse(&entry(""));

        assert_eq!(
            list,
            [Approved {
                repo: "org/name".into(),
                revision: REVISION.into(),
                worker: WorkerKind::Mlx,
                licence: "MIT".into(),
                audit: "row 3".into(),
            }]
        );
        assert!(find(&list, "org/name", REVISION).is_some());
        assert!(find(&list, "org/name", &"b".repeat(40)).is_none());
        assert!(find(&list, "org/other", REVISION).is_none());
    }

    #[test]
    fn an_entry_without_an_audit_or_with_a_bad_field_is_left_out() {
        for bad in [
            entry("").replace("audit = \"row 3\"", "audit = \" \""),
            entry("").replace("licence = \"MIT\"", "licence = \"\""),
            entry("").replace("worker = \"mlx\"", "worker = \"cuda\""),
            entry("").replace("source = \"huggingface\"", "source = \"github_release\""),
            entry("").replace(REVISION, "main"),
            entry("").replace("org/name", "../etc"),
        ] {
            assert!(parse(&bad).is_empty(), "{bad}");
        }
        let two = format!("{}{}", entry(""), entry("").replace("org/name", "org/two"));
        assert_eq!(parse(&two).len(), 2);
    }

    #[test]
    fn text_that_is_not_toml_gives_an_empty_list() {
        assert!(parse("[[model").is_empty());
        assert!(parse("model = 3").is_empty());
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        assert!(read(Path::new("/nonexistent/tts-models.toml")).is_empty());
    }

    #[test]
    fn a_file_only_the_user_can_write_is_read() {
        for mode in [0o600, 0o644, 0o400] {
            let path = scratch(mode, &entry(""));
            assert_eq!(read(&path).len(), 1, "{mode:o}");
        }
    }

    #[test]
    fn a_file_others_can_write_is_ignored() {
        for mode in [0o666, 0o620, 0o602] {
            let path = scratch(mode, &entry(""));
            assert!(read(&path).is_empty(), "{mode:o}");
        }
    }
}
