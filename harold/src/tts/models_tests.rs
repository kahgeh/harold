//! The registry on a scratch cache and data folder, with a fake Hub, a fake host and a default
//! row kept in memory.

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::settings::KOKORO_REVISION;
use crate::tts::test_support::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use tokio::time::timeout;

const REVISION: &str = "b71e4d38b236d968966a2002c4c895dbd12b1c3c";
const REPO: &str = "org/other-tts";

fn state_of(registry: &Registry, id: &str) -> InfoState {
    registry
        .list()
        .into_iter()
        .find(|model| model.id == id)
        .unwrap()
        .state
}

fn kokoro_ready(registry: &Registry) -> bool {
    registry.default_status() == DefaultStatus::Ready
}

fn kokoro_failed(registry: &Registry) -> bool {
    matches!(registry.default_status(), DefaultStatus::Failed { .. })
}

use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
async fn kokoro_with_a_record_that_holds_is_ready_and_nothing_is_asked_of_the_hub() {
    let scratch = Scratch::new();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    let registry = scratch.registry();

    registry.start_up();

    until(&registry, kokoro_ready).await;
    assert!(scratch.hub.calls().is_empty());
    let listed = registry.list();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_default);
    assert_eq!(listed[0].default_voice, "af_heart");
    assert_eq!(listed[0].voices, ["af_heart", "am_adam"]);
    assert_eq!(listed[0].state, InfoState::Ready);
}

#[tokio::test(flavor = "current_thread")]
async fn kokoro_cached_without_a_record_is_checked_and_downloads_nothing() {
    let scratch = Scratch::new();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    std::fs::remove_file(
        scratch
            .folder()
            .join(record_name(KOKORO_MLX, KOKORO_REVISION)),
    )
    .unwrap();
    scratch.hub.checks.lock().unwrap().push(Ok(300));
    let registry = scratch.registry();

    registry.start_up();

    until(&registry, kokoro_ready).await;
    assert_eq!(scratch.hub.calls(), [format!("check {KOKORO_MLX}")]);
}

#[tokio::test(flavor = "current_thread")]
async fn kokoros_record_from_the_speech_models_still_holds_after_the_upgrade_and_needs_no_network()
{
    let scratch = Scratch::new();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    // The record is where the speech models kept it, not where the TTS server looks.
    let speech_models = scratch.root.join("speech-models");
    std::fs::create_dir_all(&speech_models).unwrap();
    let name = record_name(KOKORO_MLX, KOKORO_REVISION);
    std::fs::rename(scratch.folder().join(&name), speech_models.join(&name)).unwrap();
    let registry = scratch.registry();

    registry.start_up();

    until(&registry, kokoro_ready).await;
    assert!(scratch.hub.calls().is_empty());
    assert!(scratch.folder().join(&name).exists());
}

#[tokio::test(flavor = "current_thread")]
async fn a_missing_kokoro_is_downloaded_checked_and_logged_before_it_is_listed_ready() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    let registry = scratch.registry();
    assert_eq!(registry.default_status(), DefaultStatus::Checking);

    registry.start_up();

    until(&registry, kokoro_ready).await;
    assert_eq!(
        scratch.hub.calls(),
        [
            format!("check {KOKORO_MLX}"),
            format!("download {KOKORO_MLX}")
        ]
    );
    let text = logs.text();
    assert!(text.contains("tts model download started"), "{text}");
    assert!(text.contains("tts model downloaded"), "{text}");
    assert!(text.contains(&format!("model={KOKORO_MLX}")), "{text}");
    assert!(text.contains("bytes=100"), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn until_kokoro_is_checked_a_request_is_told_it_is_downloading() {
    let scratch = Scratch::new();
    let gate = Arc::new(Notify::new());
    *scratch.hub.gate.lock().unwrap() = Some(gate.clone());
    let registry = scratch.registry();

    assert_eq!(registry.resolve("", ""), Err(ResolveError::Downloading));
    registry.start_up();
    until(&registry, |registry| {
        matches!(registry.default_status(), DefaultStatus::Downloading { .. })
    })
    .await;
    assert_eq!(registry.resolve("", ""), Err(ResolveError::Downloading));
    assert_eq!(
        registry.default_status(),
        DefaultStatus::Downloading {
            model: KOKORO_MLX.into(),
            bytes: 10,
            total: 100
        }
    );

    gate.notify_one();
    until(&registry, kokoro_ready).await;
    assert!(registry.resolve("", "").is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn a_file_that_fails_its_check_is_not_listed_ready_and_logs_check_failed() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch
        .hub
        .downloads
        .lock()
        .unwrap()
        .push(Err("check_failed"));
    let registry = scratch.registry();

    registry.start_up();

    until(&registry, kokoro_failed).await;
    assert_eq!(state_of(&registry, KOKORO_MLX), InfoState::Failed);
    assert_eq!(registry.list()[0].reason, "check_failed");
    assert_eq!(
        registry.resolve("", ""),
        Err(ResolveError::Missing("check_failed"))
    );
    let text = logs.text();
    assert!(text.contains("tts model download failed"), "{text}");
    assert!(text.contains("reason=\"check_failed\""), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_full_disk_fails_the_download_before_it_starts() {
    let scratch = Scratch::new();
    scratch.host.free.store(10, Ordering::SeqCst);
    let registry = scratch.registry();

    registry.start_up();

    until(&registry, kokoro_failed).await;
    assert_eq!(registry.list()[0].reason, "disk_full");
    assert!(
        !scratch
            .hub
            .calls()
            .iter()
            .any(|call| call.starts_with("download"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn on_the_hotspot_the_download_waits_until_a_call_starts_it() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.host.on_hotspot();
    let registry = scratch.registry();

    registry.start_up();

    until(&registry, |registry| {
        registry.default_status() == DefaultStatus::Waiting
    })
    .await;
    assert!(
        !scratch
            .hub
            .calls()
            .iter()
            .any(|call| call.starts_with("download"))
    );
    assert!(logs.text().contains("tts model download waiting"));

    registry.for_call();

    until(&registry, kokoro_ready).await;
    assert!(
        scratch
            .hub
            .calls()
            .iter()
            .any(|call| call.starts_with("download"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_tries_a_failed_download_again_unless_the_disk_was_full_or_it_is_not_allowed() {
    let scratch = Scratch::new();
    scratch.hub.downloads.lock().unwrap().push(Err("network"));
    let registry = scratch.registry();
    registry.start_up();
    until(&registry, kokoro_failed).await;

    registry.for_call();

    until(&registry, kokoro_ready).await;

    let scratch = Scratch::new();
    scratch
        .hub
        .downloads
        .lock()
        .unwrap()
        .push(Err("not_allowed"));
    let registry = scratch.registry();
    registry.start_up();
    until(&registry, kokoro_failed).await;
    let before = scratch.hub.calls().len();

    registry.for_call();
    tokio::task::yield_now().await;

    assert!(kokoro_failed(&registry));
    assert_eq!(scratch.hub.calls().len(), before);
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_that_is_not_on_the_list_is_recorded_as_waiting_for_approval() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    let registry = scratch.registry();

    let refused = registry.add_model("huggingface", REPO, REVISION).await;

    assert_eq!(refused, Err(AddRefusal::NotApproved));
    let waiting = registry.list().into_iter().find(|m| m.id == REPO).unwrap();
    assert_eq!(waiting.state, InfoState::WaitingForApproval);
    assert_eq!(waiting.reason, "not_approved");
    // Nothing was asked of the Hub: an unlisted model is never looked at.
    assert!(scratch.hub.calls().is_empty());
    let text = logs.text();
    assert!(text.contains("tts model download failed"), "{text}");
    assert!(text.contains("reason=\"not_approved\""), "{text}");
    assert_eq!(
        registry.resolve(REPO, "x"),
        Err(ResolveError::Missing("not_approved"))
    );
    // The request is kept in a file for the user only.
    let requests = scratch.folder().join("requests.json");
    assert!(std::fs::read_to_string(&requests).unwrap().contains(REPO));
    assert_eq!(
        std::fs::metadata(&requests).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test(flavor = "current_thread")]
async fn after_the_entry_is_added_to_the_list_asking_again_downloads_checks_and_lists_the_model() {
    let scratch = Scratch::new();
    scratch.safe_files();
    let registry = scratch.registry();
    assert_eq!(
        registry.add_model("huggingface", REPO, REVISION).await,
        Err(AddRefusal::NotApproved)
    );

    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    let (id, phase) = registry
        .add_model("huggingface", REPO, REVISION)
        .await
        .unwrap();

    assert_eq!(id, REPO);
    assert_eq!(phase, Phase::Checking);
    until(&registry, |registry| {
        registry
            .list()
            .iter()
            .any(|model| model.id == REPO && model.state == InfoState::Ready)
    })
    .await;
    let model = registry.list().into_iter().find(|m| m.id == REPO).unwrap();
    assert_eq!(model.size_bytes, 100);
    assert!(!model.is_default);
    // Listing the files is the format check; the download follows.
    assert!(scratch.hub.calls().contains(&format!("list {REPO}")));
    assert!(scratch.hub.calls().contains(&format!("download {REPO}")));
}

#[tokio::test(flavor = "current_thread")]
async fn a_bad_request_is_refused_before_anything_is_looked_up() {
    let scratch = Scratch::new();
    let registry = scratch.registry();

    for (source, repo, revision, refusal) in [
        ("github", REPO, REVISION, AddRefusal::UnsupportedSource),
        ("huggingface", "no-slash", REVISION, AddRefusal::InvalidRepo),
        ("huggingface", "a/../b", REVISION, AddRefusal::InvalidRepo),
        ("huggingface", REPO, "main", AddRefusal::InvalidRevision),
        (
            "huggingface",
            REPO,
            &REVISION[1..],
            AddRefusal::InvalidRevision,
        ),
        (
            "huggingface",
            REPO,
            &REVISION.to_uppercase(),
            AddRefusal::InvalidRevision,
        ),
    ] {
        assert_eq!(
            registry.add_model(source, repo, revision).await,
            Err(refusal),
            "{source} {repo} {revision}"
        );
    }
    assert!(scratch.hub.calls().is_empty());
    // A refused request is not recorded.
    assert_eq!(registry.list().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_listed_model_for_the_other_worker_is_refused() {
    let scratch = Scratch::new();
    let other = if WorkerKind::THIS_BUILD == WorkerKind::Mlx {
        "onnx"
    } else {
        "mlx"
    };
    scratch.approve(REPO, REVISION, other);
    let registry = scratch.registry();

    let refused = registry.add_model("huggingface", REPO, REVISION).await;

    assert_eq!(refused, Err(AddRefusal::UnsupportedWorker));
    assert_eq!(registry.list().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_with_a_weight_file_that_can_run_code_is_refused_unsafe_format() {
    for unsafe_file in ["pytorch_model.bin", "model.pt", "weights.pth", "x.pkl"] {
        let logs = CapturedLogs::start();
        let scratch = Scratch::new();
        scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
        *scratch.hub.files.lock().unwrap() = vec![
            RemoteFile {
                path: "model.safetensors".into(),
                size: 5,
            },
            RemoteFile {
                path: unsafe_file.into(),
                size: 5,
            },
        ];
        let registry = scratch.registry();

        let refused = registry.add_model("huggingface", REPO, REVISION).await;

        assert_eq!(refused, Err(AddRefusal::UnsafeFormat), "{unsafe_file}");
        assert!(logs.text().contains("reason=\"unsafe_format\""));
        assert!(
            !scratch
                .hub
                .calls()
                .iter()
                .any(|call| call.starts_with("download"))
        );
        assert_eq!(registry.list().len(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_with_no_safe_weight_file_is_refused_and_each_safe_format_is_accepted() {
    for (files, accepted) in [
        (vec!["config.json"], false),
        (vec!["model.onnx"], true),
        (vec!["model.gguf", "tokenizer.json"], true),
        (vec!["a.safetensors"], true),
        (vec!["A.SAFETENSORS"], true),
        (vec!["model.BIN", "a.safetensors"], false),
    ] {
        let scratch = Scratch::new();
        scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
        *scratch.hub.files.lock().unwrap() = files
            .iter()
            .map(|path| RemoteFile {
                path: path.to_string(),
                size: 1,
            })
            .collect();
        let registry = scratch.registry();

        let result = registry.add_model("huggingface", REPO, REVISION).await;

        assert_eq!(result.is_ok(), accepted, "{files:?}");
        if !accepted {
            assert_eq!(result, Err(AddRefusal::UnsafeFormat), "{files:?}");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_hub_that_cannot_be_asked_refuses_with_its_reason() {
    let scratch = Scratch::new();
    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    *scratch.hub.list_error.lock().unwrap() = Some("network");
    let registry = scratch.registry();

    assert_eq!(
        registry.add_model("huggingface", REPO, REVISION).await,
        Err(AddRefusal::Unavailable("network"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn models_asked_for_before_come_back_after_a_restart() {
    let scratch = Scratch::new();
    scratch.safe_files();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    let first = scratch.registry();
    first
        .add_model("huggingface", REPO, REVISION)
        .await
        .unwrap();
    until(&first, |registry| {
        registry
            .list()
            .iter()
            .any(|model| model.id == REPO && model.state == InfoState::Ready)
    })
    .await;
    assert_eq!(
        first
            .add_model("huggingface", "org/unlisted", REVISION)
            .await,
        Err(AddRefusal::NotApproved)
    );
    let before = scratch.hub.calls().len();

    // A restart: the same folder, a new registry. The record of the check still holds.
    scratch.cached(REPO, REVISION, 50);
    let second = scratch.registry();
    second.start_up();

    until(&second, |registry| {
        registry
            .list()
            .iter()
            .filter(|model| model.state == InfoState::Ready)
            .count()
            == 2
    })
    .await;
    let states: Vec<_> = second.list().into_iter().map(|m| (m.id, m.state)).collect();
    assert!(states.contains(&("org/unlisted".to_string(), InfoState::WaitingForApproval)));
    assert_eq!(scratch.hub.calls().len(), before);
}

#[tokio::test(flavor = "current_thread")]
async fn kokoro_and_the_default_cannot_be_removed_and_another_model_can() {
    let scratch = Scratch::new();
    scratch.safe_files();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    let registry = scratch.registry();
    registry.start_up();
    until(&registry, kokoro_ready).await;
    registry
        .add_model("huggingface", REPO, REVISION)
        .await
        .unwrap();
    until(&registry, |registry| {
        state_of(registry, REPO) == InfoState::Ready
    })
    .await;

    assert_eq!(
        registry.remove_model(KOKORO_MLX),
        Err(RemoveRefusal::DefaultModel)
    );
    assert_eq!(
        registry.remove_model("org/none"),
        Err(RemoveRefusal::UnknownModel)
    );
    registry.set_default(REPO, "af_heart").await.unwrap();
    assert_eq!(
        registry.remove_model(REPO),
        Err(RemoveRefusal::DefaultModel)
    );
    registry.set_default(KOKORO_MLX, "af_heart").await.unwrap();

    assert_eq!(registry.remove_model(REPO), Ok(()));
    assert!(registry.list().iter().all(|model| model.id != REPO));
    assert!(!scratch.folder().join(record_name(REPO, REVISION)).exists());
    let requests = std::fs::read_to_string(scratch.folder().join("requests.json")).unwrap();
    assert!(!requests.contains(REPO));
}

#[tokio::test(flavor = "current_thread")]
async fn the_default_survives_a_restart() {
    let scratch = Scratch::new();
    scratch.safe_files();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    let registry = scratch.registry();
    registry
        .add_model("huggingface", REPO, REVISION)
        .await
        .unwrap();
    until(&registry, |registry| {
        state_of(registry, REPO) == InfoState::Ready
    })
    .await;

    registry.set_default(REPO, "voice_one").await.unwrap();

    assert_eq!(
        registry.default_pair(),
        (REPO.to_string(), "voice_one".to_string())
    );
    let after = scratch.registry();
    assert_eq!(after.default_pair().0, KOKORO_MLX);
    after.load_default().await;
    assert_eq!(
        after.default_pair(),
        (REPO.to_string(), "voice_one".to_string())
    );
    assert!(
        after
            .list()
            .iter()
            .find(|m| m.id == REPO)
            .unwrap()
            .is_default
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_stored_default_for_a_model_that_is_gone_is_ignored() {
    let scratch = Scratch::new();
    *scratch.default_row.0.lock().unwrap() = Some(("org/gone".into(), "v".into()));
    let registry = scratch.registry();

    registry.load_default().await;

    assert_eq!(registry.default_pair().0, KOKORO_MLX);
}

#[tokio::test(flavor = "current_thread")]
async fn set_default_refuses_an_unknown_model_voice_or_unapproved_model() {
    let scratch = Scratch::new();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    let registry = scratch.registry();
    registry.start_up();
    until(&registry, kokoro_ready).await;
    registry
        .add_model("huggingface", REPO, REVISION)
        .await
        .unwrap_err();

    assert_eq!(
        registry.set_default("org/none", "af_heart").await,
        Err(SetDefaultRefusal::UnknownModel)
    );
    assert_eq!(
        registry.set_default(KOKORO_MLX, "nobody").await,
        Err(SetDefaultRefusal::UnknownVoice)
    );
    assert_eq!(
        registry.set_default(KOKORO_MLX, "").await,
        Err(SetDefaultRefusal::UnknownVoice)
    );
    assert_eq!(
        registry.set_default(REPO, "v").await,
        Err(SetDefaultRefusal::NotReady)
    );
    assert_eq!(registry.set_default(KOKORO_MLX, "am_adam").await, Ok(()));
    assert_eq!(
        scratch.default_row.0.lock().unwrap().clone(),
        Some((KOKORO_MLX.to_string(), "am_adam".to_string()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_resolves_to_the_model_voice_and_worker_files() {
    let scratch = Scratch::new();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    let registry = scratch.registry();
    registry.start_up();
    until(&registry, kokoro_ready).await;

    let resolved = registry.resolve("", "").unwrap();

    assert_eq!(resolved.id, KOKORO_MLX);
    assert_eq!(resolved.voice, "af_heart");
    assert_eq!(
        resolved.load.kind(),
        WorkerKind::THIS_BUILD,
        "{:?}",
        resolved.load
    );
    assert_eq!(
        registry.resolve(KOKORO_MLX, "am_adam").unwrap().voice,
        "am_adam"
    );
    assert_eq!(
        registry.resolve("org/none", ""),
        Err(ResolveError::UnknownModel)
    );
    assert_eq!(
        registry.resolve(KOKORO_MLX, "nobody"),
        Err(ResolveError::UnknownVoice)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn two_downloads_never_run_at_once() {
    let scratch = Scratch::new();
    scratch.safe_files();
    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    scratch.approve("org/third", REVISION, WorkerKind::THIS_BUILD.as_str());
    let gate = Arc::new(Notify::new());
    *scratch.hub.gate.lock().unwrap() = Some(gate.clone());
    let registry = scratch.registry();
    registry.start_up();
    registry
        .add_model("huggingface", REPO, REVISION)
        .await
        .unwrap();
    registry
        .add_model("huggingface", "org/third", REVISION)
        .await
        .unwrap();

    for _ in 0..3 {
        timeout(Duration::from_secs(10), async {
            while scratch.hub.running.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.notify_one();
        tokio::task::yield_now().await;
    }
    until(&registry, |registry| {
        registry
            .list()
            .iter()
            .all(|model| model.state == InfoState::Ready)
    })
    .await;

    assert_eq!(scratch.hub.most_running.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_download_waits_while_another_holds_the_machines_download_slot() {
    let scratch = Scratch::new();
    let registry = scratch.registry();
    let slot = download_slot().lock().await;

    registry.start_up();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        !scratch
            .hub
            .calls()
            .iter()
            .any(|call| call.starts_with("download"))
    );

    drop(slot);
    until(&registry, kokoro_ready).await;
}

#[test]
fn kokoro_for_this_build_is_pinned_and_a_refused_revision_is_not_managed() {
    let tts = TtsSettings::default();
    let row = kokoro_row(&tts);
    match WorkerKind::THIS_BUILD {
        WorkerKind::Mlx => {
            assert_eq!(row.id, "mlx-community/Kokoro-82M-bf16");
            assert_eq!(row.revision, KOKORO_REVISION);
            assert_eq!(row.source, Source::HuggingFace);
            assert_eq!(row.size, 389_410_807);
            let bad = TtsSettings {
                model_revision: "main".into(),
                ..TtsSettings::default()
            };
            assert_eq!(kokoro_row(&bad).phase, Phase::Failed("not_allowed"));
        }
        WorkerKind::Onnx => {
            assert_eq!(row.id, "thewh1teagle/kokoro-onnx");
            assert_eq!(row.source, Source::GithubRelease);
            assert_eq!(row.revision, "model-files-v1.0");
        }
    }
    assert_eq!(row.phase, Phase::Checking);
}

#[test]
fn the_safe_format_rule_looks_at_extensions_only() {
    let files = |paths: &[&str]| -> Vec<RemoteFile> {
        paths
            .iter()
            .map(|path| RemoteFile {
                path: path.to_string(),
                size: 1,
            })
            .collect()
    };
    assert!(has_only_safe_weights(&files(&[
        "a/b/model.safetensors",
        "README.md"
    ])));
    assert!(!has_only_safe_weights(&files(&[
        "model.safetensors",
        "dir/weights.ckpt"
    ])));
    assert!(!has_only_safe_weights(&files(&["README.md"])));
    assert!(!has_only_safe_weights(&[]));
}

#[test]
fn the_release_row_is_found_in_its_own_folder() {
    let scratch = Scratch::new();
    let row = Row {
        id: release_download::MODEL_ID.to_string(),
        source: Source::GithubRelease,
        revision: release_download::RELEASE_TAG.to_string(),
        worker: WorkerKind::Onnx,
        phase: Phase::Ready,
        size: 1,
        voices: Vec::new(),
        asked: false,
    };
    let registry = Registry::new(scratch.deps(), row.clone());

    let load = registry.load_of(&row).unwrap();

    let folder = scratch.folder().join("thewh1teagle--kokoro-onnx");
    assert_eq!(
        load,
        Load::Onnx {
            model: release_download::MODEL_ID.to_string(),
            model_file: folder.join("kokoro-v1.0.onnx"),
            voices_file: folder.join("voices-v1.0.bin"),
        }
    );
}

#[test]
fn a_hugging_face_onnx_row_loads_its_snapshot_files_with_the_rows_id() {
    let scratch = Scratch::new();
    let row = Row {
        id: "someone/other-onnx".to_string(),
        source: Source::HuggingFace,
        revision: "a".repeat(40),
        worker: WorkerKind::Onnx,
        phase: Phase::Ready,
        size: 1,
        voices: Vec::new(),
        asked: false,
    };
    let registry = Registry::new(scratch.deps(), row.clone());
    let snapshot = snapshot_dir(&scratch.cache(), &row.id, &row.revision);
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(snapshot.join("m.onnx"), b"a").unwrap();
    std::fs::write(snapshot.join("voices.bin"), b"b").unwrap();

    let load = registry.load_of(&row).unwrap();

    assert_eq!(
        load,
        Load::Onnx {
            model: "someone/other-onnx".to_string(),
            model_file: snapshot.join("m.onnx"),
            voices_file: snapshot.join("voices.bin"),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_release_row_with_a_record_that_holds_is_ready_without_a_download() {
    let scratch = Scratch::new();
    let row = Row {
        id: release_download::MODEL_ID.to_string(),
        source: Source::GithubRelease,
        revision: release_download::RELEASE_TAG.to_string(),
        worker: WorkerKind::Onnx,
        phase: Phase::Checking,
        size: 7,
        voices: Vec::new(),
        asked: false,
    };
    let folder = scratch.folder().join("thewh1teagle--kokoro-onnx");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("kokoro-v1.0.onnx"), b"abcd").unwrap();
    std::fs::write(folder.join("voices-v1.0.bin"), b"xyz").unwrap();
    std::fs::write(
        scratch.folder().join(record_name(&row.id, &row.revision)),
        json!({"files": [
            {"path": "kokoro-v1.0.onnx", "size": 4},
            {"path": "voices-v1.0.bin", "size": 3},
        ]})
        .to_string(),
    )
    .unwrap();
    let registry = Registry::new(scratch.deps(), row);

    registry.start_up();

    until(&registry, kokoro_ready).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_release_row_whose_download_fails_its_check_is_listed_failed_with_check_failed() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    // A local server that answers every request with five bytes, which is not the pinned size.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nwrong",
                    )
                    .await;
            });
        }
    });
    let mut deps = scratch.deps();
    deps.release = Arc::new(release_download::ReleaseDownloader::for_test(
        format!("http://127.0.0.1:{port}/release"),
        release_download::FILES.to_vec(),
        vec!["127.0.0.1".to_string()],
    ));
    let row = Row {
        id: release_download::MODEL_ID.to_string(),
        source: Source::GithubRelease,
        revision: release_download::RELEASE_TAG.to_string(),
        worker: WorkerKind::Onnx,
        phase: Phase::Checking,
        size: 7,
        voices: Vec::new(),
        asked: false,
    };
    let registry = Registry::new(deps, row);

    registry.start_up();

    until(&registry, kokoro_failed).await;
    assert_eq!(registry.list()[0].reason, "check_failed");
    assert_eq!(
        registry.resolve("", ""),
        Err(ResolveError::Missing("check_failed"))
    );
    assert!(logs.text().contains("tts model download failed"));
}

#[tokio::test(flavor = "current_thread")]
async fn voices_a_worker_reported_are_listed() {
    let scratch = Scratch::new();
    let registry = scratch.registry();

    registry.note_voices(KOKORO_MLX, &["a".to_string(), "b".to_string()]);

    assert_eq!(registry.list()[0].voices, ["a", "b"]);
}

#[test]
fn the_hand_command_downloads_the_pinned_commit_without_a_token() {
    let scratch = Scratch::new();
    let registry = scratch.registry();

    let command = registry
        .hand_command("/venv/bin/python", KOKORO_MLX, KOKORO_REVISION)
        .unwrap();

    assert!(command.contains(&format!(
        "snapshot_download('{KOKORO_MLX}', revision='{KOKORO_REVISION}', token=False)"
    )));
    assert!(
        registry
            .hand_command("/p", "bad name", KOKORO_REVISION)
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_that_waited_for_approval_is_format_checked_after_a_restart() {
    let scratch = Scratch::new();
    scratch.cached(KOKORO_MLX, KOKORO_REVISION, 100);
    let first = scratch.registry();
    assert_eq!(
        first.add_model("huggingface", REPO, REVISION).await,
        Err(AddRefusal::NotApproved)
    );
    // The entry is added, and Harold restarts. The repository holds a pickle file.
    scratch.approve(REPO, REVISION, WorkerKind::THIS_BUILD.as_str());
    *scratch.hub.files.lock().unwrap() = vec![
        RemoteFile {
            path: "model.safetensors".into(),
            size: 5,
        },
        RemoteFile {
            path: "weights.pkl".into(),
            size: 5,
        },
    ];
    let second = scratch.registry();

    second.start_up();

    until(&second, |registry| {
        state_of(registry, REPO) == InfoState::Failed
    })
    .await;
    let listed = second.list().into_iter().find(|m| m.id == REPO).unwrap();
    assert_eq!(listed.reason, "unsafe_format");
    assert!(
        !scratch
            .hub
            .calls()
            .iter()
            .any(|call| call.starts_with("download"))
    );
    assert!(
        !scratch
            .hub
            .calls()
            .iter()
            .any(|call| call.starts_with("check"))
    );
}
