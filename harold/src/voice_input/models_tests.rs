//! The speech models on a scratch cache and data folder, with a fake download program: a
//! shell script that records how it was started and says what the test tells it to.

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::settings::{KOKORO_REVISION, WHISPER_REVISION};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

struct FakeHost {
    free: AtomicU64,
    gateway: Mutex<Option<Ipv4Addr>>,
}

impl FakeHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            free: AtomicU64::new(u64::MAX),
            gateway: Mutex::new(Some(Ipv4Addr::new(192, 168, 68, 1))),
        })
    }

    fn on_hotspot(&self) {
        *self.gateway.lock().unwrap() = Some(Ipv4Addr::new(172, 20, 10, 1));
    }
}

impl Host for FakeHost {
    fn free_bytes<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, Option<u64>> {
        Box::pin(async move { Some(self.free.load(Ordering::SeqCst)) })
    }

    fn gateway(&self) -> BoxFuture<'_, Option<Ipv4Addr>> {
        Box::pin(async move { *self.gateway.lock().unwrap() })
    }
}

/// A scratch Harold: a cache, a data folder, a fake download program and its log.
struct Scratch {
    root: PathBuf,
    host: Arc<FakeHost>,
}

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("harold-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("cache")).unwrap();
        Self {
            root,
            host: FakeHost::new(),
        }
    }

    fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn folder(&self) -> PathBuf {
        self.root.join("speech-models")
    }

    fn log(&self) -> PathBuf {
        self.root.join("runs.log")
    }

    /// How many times the download program ran.
    fn runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn env(&self) -> Env {
        Env {
            path: "/usr/bin:/bin".into(),
            home: self.root.join("home"),
            hf_home: None,
            hf_hub_cache: Some(self.cache().display().to_string()),
        }
    }

    /// A fake `python`: logs its arguments (one line for each run) and the variables it was
    /// given, then runs `body`.
    fn fake_python(&self, body: &str) -> String {
        let script = self.root.join("fake-python");
        let text = format!(
            "#!/bin/sh\necho \"$*\" >> {log}\nenv > {log}.env\n{body}\n",
            log = self.log().display()
        );
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script.display().to_string()
    }

    fn models(&self, python: &str) -> Arc<SpeechModels> {
        self.models_for(python, WHISPER, WHISPER_REVISION, KOKORO_REVISION)
    }

    fn models_for(
        &self,
        python: &str,
        whisper: &str,
        whisper_revision: &str,
        kokoro_revision: &str,
    ) -> Arc<SpeechModels> {
        let voice_input = VoiceInputSettings {
            model_revision: whisper_revision.to_string(),
            keep_worker_minutes: 10,
            command: python.to_string(),
            args: ["--model", whisper].map(String::from).to_vec(),
        };
        let tts = TtsSettings {
            model_revision: kokoro_revision.to_string(),
            command: "say".into(),
            voice: None,
            args: None,
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
        };
        SpeechModels::new(
            &voice_input,
            &tts,
            self.folder(),
            self.env(),
            Arc::clone(&self.host) as Arc<dyn Host>,
        )
    }

    /// Puts a snapshot of `model` at `revision` in the cache with a file of `bytes` bytes.
    fn snapshot(&self, model: &str, revision: &str, bytes: usize) {
        let dir = snapshot_dir(&self.cache(), model, revision);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("weights.safetensors"), vec![7u8; bytes]).unwrap();
    }

    /// Writes the record of a check that listed a file of `bytes` bytes.
    fn record(&self, model: &str, revision: &str, bytes: usize) {
        std::fs::create_dir_all(self.folder()).unwrap();
        let name = format!("{}@{revision}.json", model.replace('/', "--"));
        let text = format!(
            r#"{{"model":"{model}","commit":"{revision}","files":[{{"path":"weights.safetensors","size":{bytes},"sha256":"x"}}]}}"#
        );
        std::fs::write(self.folder().join(name), text).unwrap();
    }

    /// Both models cached and recorded.
    fn both_checked(&self) {
        for (model, revision) in [(WHISPER, WHISPER_REVISION), (KOKORO, KOKORO_REVISION)] {
            self.snapshot(model, revision, 100);
            self.record(model, revision, 100);
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Waits for a status the test is looking for, with a real-time limit.
async fn until(models: &Arc<SpeechModels>, wanted: impl Fn(&Status) -> bool) -> Status {
    let mut status = models.status();
    let seen = timeout(
        Duration::from_secs(20),
        status.wait_for(|status| wanted(status)),
    )
    .await
    .expect("the status arrives in time")
    .expect("the models stay");
    seen.clone()
}

fn finished(status: &Status) -> bool {
    matches!(status, Status::Ready | Status::Failed(_))
}

/// What the fake download program prints for a download or a check that works.
const WORKS: &str = r#"echo '{"total": 2000}'; echo '{"done": true, "bytes": 2000}'"#;

fn failing(reason: &str) -> String {
    format!(r#"echo '{{"total": 2000}}'; echo '{{"done": false, "reason": "{reason}"}}'"#)
}

#[test]
fn a_revision_is_40_lowercase_hex_characters() {
    assert!(is_revision(WHISPER_REVISION));
    assert!(!is_revision(&WHISPER_REVISION[..39]));
    assert!(!is_revision(&WHISPER_REVISION.to_uppercase()));
    assert!(!is_revision(&format!("{}g", &WHISPER_REVISION[..39])));
    assert!(!is_revision(&format!("{} ", &WHISPER_REVISION[..39])));
    assert!(!is_revision("main"));
}

#[test]
fn a_model_is_org_slash_name_in_letters_digits_dot_underscore_and_dash() {
    assert!(is_model_name(WHISPER));
    assert!(is_model_name("a.b_c/D-1"));
    for bad in [
        "", "no-slash", "a/b/c", "a/", "/b", "a b/c", "a;b/c", "a/b;c", "a/b c", "a/$(x)", "../x",
        "a/b\n",
    ] {
        assert!(!is_model_name(bad), "{bad:?}");
    }
}

#[test]
fn the_cache_is_hub_cache_then_hf_home_then_the_home_cache() {
    let mut env = Env {
        path: "/bin".into(),
        home: "/Users/me".into(),
        hf_home: None,
        hf_hub_cache: None,
    };
    assert_eq!(env.cache(), Path::new("/Users/me/.cache/huggingface/hub"));
    env.hf_home = Some("/hf".into());
    assert_eq!(env.cache(), Path::new("/hf/hub"));
    env.hf_hub_cache = Some("/cache".into());
    assert_eq!(env.cache(), Path::new("/cache"));
    assert_eq!(
        snapshot_dir(&env.cache(), WHISPER, WHISPER_REVISION),
        Path::new("/cache")
            .join("models--mlx-community--whisper-large-v3-turbo/snapshots")
            .join(WHISPER_REVISION)
    );
}

#[test]
fn only_the_iphone_hotspot_range_is_metered() {
    assert!(is_hotspot(Ipv4Addr::new(172, 20, 10, 1)));
    assert!(is_hotspot(Ipv4Addr::new(172, 20, 10, 0)));
    assert!(is_hotspot(Ipv4Addr::new(172, 20, 10, 15)));
    assert!(!is_hotspot(Ipv4Addr::new(172, 20, 10, 16)));
    assert!(!is_hotspot(Ipv4Addr::new(172, 20, 11, 1)));
    assert!(!is_hotspot(Ipv4Addr::new(192, 168, 68, 1)));
}

#[test]
fn a_pin_is_made_only_for_the_two_models_at_a_full_commit() {
    let env = Env {
        path: "/bin".into(),
        home: "/h".into(),
        hf_home: None,
        hf_hub_cache: Some("/cache".into()),
    };
    let pinned = pin(KOKORO, KOKORO_REVISION, &env).unwrap();
    assert_eq!(pinned.commit, KOKORO_REVISION);
    assert_eq!(
        pinned.snapshot,
        snapshot_dir(Path::new("/cache"), KOKORO, KOKORO_REVISION)
    );
    assert!(pin("mlx-community/other", KOKORO_REVISION, &env).is_none());
    assert!(pin(KOKORO, "main", &env).is_none());
    assert!(pin("evil;rm/x", KOKORO_REVISION, &env).is_none());
}

#[test]
fn the_notice_names_the_model_the_reason_and_the_command() {
    let scratch = Scratch::new();
    let models = scratch.models("/venv/bin/python");
    let missing = models.missing(WHISPER, "network");
    assert_eq!(
        missing.notice(),
        format!(
            "The speech model {WHISPER} didn't download (network). To get it, run on the Mac: \
             `/venv/bin/python -c \"from huggingface_hub import snapshot_download; \
             snapshot_download('{WHISPER}', revision='{WHISPER_REVISION}', token=False)\"`, \
             then call again."
        )
    );
    // A model or commit that is not fit to put in a command gets no command.
    let odd = models.missing("a;b/c", "not_allowed");
    assert_eq!(odd.command, None);
    assert_eq!(
        odd.notice(),
        "The speech model a;b/c didn't download (not_allowed)."
    );
}

#[test]
fn progress_is_whole_percent_and_never_over_100() {
    let downloading = |bytes, total| Status::Downloading {
        model: WHISPER.into(),
        bytes,
        total,
    };
    assert_eq!(downloading(450, 1000).percent(), Some(45));
    assert_eq!(downloading(0, 1000).percent(), Some(0));
    assert_eq!(downloading(5000, 1000).percent(), Some(100));
    assert_eq!(downloading(1, 0).percent(), Some(100));
    assert_eq!(Status::Ready.percent(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn a_record_whose_files_are_all_there_passes_with_no_script_run() {
    let scratch = Scratch::new();
    scratch.both_checked();
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
    assert!(scratch.runs().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_missing_record_runs_the_script_in_its_check_mode_and_downloads_nothing() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.snapshot(WHISPER, WHISPER_REVISION, 100);
    scratch.record(WHISPER, WHISPER_REVISION, 100);
    scratch.snapshot(KOKORO, KOKORO_REVISION, 100);
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
    let runs = scratch.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        runs[0].starts_with(&format!(
            "-I {}/voice_download.py {KOKORO} {KOKORO_REVISION} ",
            scratch.folder().display()
        )),
        "{runs:?}"
    );
    assert!(runs[0].ends_with(" --check"), "{runs:?}");
    assert!(!logs.text().contains("speech model download started"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_check_that_finds_a_file_missing_starts_the_download_and_logs_it() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.snapshot(WHISPER, WHISPER_REVISION, 100);
    scratch.record(WHISPER, WHISPER_REVISION, 100);
    scratch.both_checked();
    // The first run is the check, which finds the file missing; the second downloads.
    let python = scratch.fake_python(&format!(
        "case \"$*\" in *--check*) echo '{{\"done\": false, \"reason\": \"missing\"}}';; *) {WORKS};; esac"
    ));
    std::fs::remove_file(scratch.folder().join(format!(
        "{}@{KOKORO_REVISION}.json",
        KOKORO.replace('/', "--")
    )))
    .unwrap();
    let models = scratch.models(&python);

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
    let runs = scratch.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs[0].ends_with(" --check"));
    assert!(!runs[1].contains("--check"));
    let text = logs.text();
    assert!(text.contains(&format!(
        "speech model download started model={KOKORO} total_bytes=389410807"
    )));
    assert!(text.contains(&format!(
        "speech model downloaded model={KOKORO} bytes=2000 ms="
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn a_short_file_in_the_snapshot_starts_the_download_with_no_check_first() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.both_checked();
    // The recorded size is no longer the file's.
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
    let runs = scratch.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(!runs[0].contains("--check"), "{runs:?}");
    assert!(runs[0].contains(&format!("{WHISPER} {WHISPER_REVISION}")));
    let text = logs.text();
    assert!(text.contains("speech model download started"), "{text}");
    assert!(text.contains("speech model downloaded"), "{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_missing_snapshot_file_starts_the_download() {
    let scratch = Scratch::new();
    scratch.both_checked();
    std::fs::remove_file(
        snapshot_dir(&scratch.cache(), KOKORO, KOKORO_REVISION).join("weights.safetensors"),
    )
    .unwrap();
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
    assert_eq!(scratch.runs().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_download_is_logged_with_the_scripts_reason_and_the_model_is_missing() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let models = scratch.models(&scratch.fake_python(&failing("network")));

    models.start_up();

    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the download failed");
    };
    assert_eq!(missing.model, WHISPER);
    assert_eq!(missing.reason, "network");
    assert!(
        logs.text().contains(&format!(
            "speech model not downloaded model={WHISPER} reason=\"network\""
        )) || logs.text().contains(&format!(
            "speech model not downloaded model={WHISPER} reason=network"
        )),
        "{}",
        logs.text()
    );
    assert!(logs.text().contains("WARN"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_reason_the_script_makes_up_is_failed() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let models = scratch.models(&scratch.fake_python(&failing("rm -rf /")));

    models.start_up();

    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the download failed");
    };
    assert_eq!(missing.reason, "failed");
}

#[tokio::test(flavor = "current_thread")]
async fn a_script_that_prints_nothing_or_dies_is_a_failed_download() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let models = scratch.models(&scratch.fake_python("exit 3"));

    models.start_up();

    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the download failed");
    };
    assert_eq!(missing.reason, "failed");
}

#[tokio::test(flavor = "current_thread")]
async fn low_disk_logs_disk_full_without_starting() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    // The model's size and 1 GB are not free.
    scratch
        .host
        .free
        .store(1_613_979_758 + 999_999_999, Ordering::SeqCst);
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the disk is full");
    };
    assert_eq!(missing.reason, "disk_full");
    assert!(scratch.runs().is_empty());
    let text = logs.text();
    assert!(text.contains("speech model not downloaded"), "{text}");
    assert!(text.contains("disk_full"), "{text}");
    assert!(!text.contains("speech model download started"));
}

#[tokio::test(flavor = "current_thread")]
async fn enough_disk_starts_the_download() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    scratch
        .host
        .free
        .store(1_613_979_758 + 1_000_000_000, Ordering::SeqCst);
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
}

#[tokio::test(flavor = "current_thread")]
async fn a_gateway_in_the_hotspot_range_holds_the_startup_download_back_until_a_call() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    scratch.host.on_hotspot();
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();
    assert_eq!(
        until(&models, |s| *s == Status::Waiting).await,
        Status::Waiting
    );
    assert!(scratch.runs().is_empty());
    let text = logs.text();
    assert_eq!(
        text.matches("speech model download waiting").count(),
        1,
        "{text}"
    );
    assert!(
        text.contains("reason=\"metered\"") || text.contains("reason=metered"),
        "{text}"
    );

    // A tapped call starts the download at once, on the hotspot.
    models.for_call();
    assert_eq!(until(&models, finished).await, Status::Ready);
    assert_eq!(scratch.runs().len(), 1);
    assert!(logs.text().contains("speech model downloaded"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_gateway_outside_the_hotspot_range_starts_the_download() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let models = scratch.models(&scratch.fake_python(WORKS));

    models.start_up();

    assert_eq!(until(&models, finished).await, Status::Ready);
    assert_eq!(scratch.runs().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn the_metered_wait_looks_again_every_ten_minutes_and_logs_once() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    scratch.host.on_hotspot();
    let models = scratch.models(&scratch.fake_python(WORKS));
    models.start_up();
    until(&models, |s| *s == Status::Waiting).await;

    // The Mac leaves the hotspot; the next look, ten minutes on, starts the download.
    *scratch.host.gateway.lock().unwrap() = Some(Ipv4Addr::new(192, 168, 68, 1));
    tokio::time::pause();
    tokio::time::advance(METERED_RECHECK + Duration::from_secs(1)).await;
    tokio::time::resume();

    assert_eq!(until(&models, finished).await, Status::Ready);
    assert_eq!(
        logs.text().matches("speech model download waiting").count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_after_a_failed_download_starts_it_again_unless_the_disk_or_a_permission_failed() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let python = scratch.fake_python(&format!(
        "if [ -e {root}/second ]; then {WORKS}; else touch {root}/second; {fail}; fi",
        root = scratch.root.display(),
        fail = failing("network")
    ));
    let models = scratch.models(&python);
    models.start_up();
    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the first download failed");
    };
    assert_eq!(missing.reason, "network");

    models.for_call();
    assert_eq!(until(&models, finished).await, Status::Ready);
    assert_eq!(scratch.runs().len(), 2);

    // A disk that is full, or a download not allowed, ends the call at once instead.
    for reason in ["disk_full", "not_allowed"] {
        let scratch = Scratch::new();
        scratch.both_checked();
        scratch.record(WHISPER, WHISPER_REVISION, 999);
        let models = scratch.models(&scratch.fake_python(&failing(reason)));
        models.start_up();
        let Status::Failed(first) = until(&models, finished).await else {
            panic!("the download failed");
        };
        assert_eq!(first.reason, reason);
        models.for_call();
        let Status::Failed(again) = until(&models, finished).await else {
            panic!("the status stays failed");
        };
        assert_eq!(again.reason, reason);
        assert_eq!(scratch.runs().len(), 1, "{reason}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_outside_the_two_a_bad_name_or_a_short_revision_is_refused_and_never_run() {
    let cases = [
        (WHISPER, "main", KOKORO_REVISION),
        (WHISPER, &WHISPER_REVISION[..39], KOKORO_REVISION),
        (
            "mlx-community/whisper-small",
            WHISPER_REVISION,
            KOKORO_REVISION,
        ),
        (
            "mlx-community/x;touch pwned",
            WHISPER_REVISION,
            KOKORO_REVISION,
        ),
        ("evil org/model", WHISPER_REVISION, KOKORO_REVISION),
        (WHISPER, WHISPER_REVISION, "abc"),
    ];
    for (whisper, whisper_revision, kokoro_revision) in cases {
        let logs = CapturedLogs::start();
        let scratch = Scratch::new();
        scratch.both_checked();
        let models = scratch.models_for(
            &scratch.fake_python(WORKS),
            whisper,
            whisper_revision,
            kokoro_revision,
        );

        models.start_up();

        assert_eq!(until(&models, finished).await, Status::Ready, "{whisper}");
        assert!(scratch.runs().is_empty(), "{whisper}");
        assert!(!scratch.folder().join("pwned").exists());
        assert!(!scratch.root.join("pwned").exists());
        let refused = logs.text().contains("speech model setting refused");
        // A model outside the list with a good name and commit is not refused, only unmanaged.
        let outside_list = whisper == "mlx-community/whisper-small";
        assert_eq!(
            refused, !outside_list,
            "{whisper} {whisper_revision} {kokoro_revision}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_the_worker_cannot_find_that_is_not_one_of_the_two_is_reported_missing_not_downloaded()
 {
    let scratch = Scratch::new();
    scratch.both_checked();
    let models = scratch.models_for(
        &scratch.fake_python(WORKS),
        "mlx-community/whisper-small",
        WHISPER_REVISION,
        KOKORO_REVISION,
    );
    models.start_up();
    until(&models, finished).await;

    models.model_missing("mlx-community/whisper-small");

    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the model is missing");
    };
    assert_eq!(missing.model, "mlx-community/whisper-small");
    assert_eq!(missing.reason, "not_allowed");
    assert!(missing.command.is_some());
    assert!(scratch.runs().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_the_worker_cannot_find_is_checked_and_downloaded_again() {
    let scratch = Scratch::new();
    scratch.both_checked();
    let models = scratch.models(&scratch.fake_python(WORKS));
    models.start_up();
    assert_eq!(until(&models, finished).await, Status::Ready);
    assert!(scratch.runs().is_empty());

    // The cache lost the file after the check.
    std::fs::remove_file(
        snapshot_dir(&scratch.cache(), WHISPER, WHISPER_REVISION).join("weights.safetensors"),
    )
    .unwrap();
    models.model_missing(WHISPER);

    assert_eq!(
        until(&models, |s| *s == Status::Checking).await,
        Status::Checking
    );
    assert_eq!(until(&models, finished).await, Status::Ready);
    assert_eq!(scratch.runs().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn the_script_is_started_with_dash_i_and_only_the_four_variables() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let mut env = scratch.env();
    env.hf_home = Some(scratch.root.join("hf").display().to_string());
    let models = {
        let voice_input = VoiceInputSettings {
            model_revision: WHISPER_REVISION.into(),
            keep_worker_minutes: 10,
            command: scratch.fake_python(WORKS),
            args: ["--model", WHISPER].map(String::from).to_vec(),
        };
        let tts = TtsSettings {
            model_revision: KOKORO_REVISION.into(),
            command: "say".into(),
            voice: None,
            args: None,
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
        };
        SpeechModels::new(
            &voice_input,
            &tts,
            scratch.folder(),
            env,
            Arc::clone(&scratch.host) as Arc<dyn Host>,
        )
    };
    // Harold's own environment holds more, which the script must not get.
    // SAFETY: no other test reads this variable.
    unsafe { std::env::set_var("HAROLD_TEST_SECRET", "token-value") };

    models.start_up();
    assert_eq!(until(&models, finished).await, Status::Ready);

    let runs = scratch.runs();
    assert!(runs[0].starts_with("-I "), "{runs:?}");
    let env = std::fs::read_to_string(format!("{}.env", scratch.log().display())).unwrap();
    let names: std::collections::BTreeSet<&str> = env
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    for given in ["PATH", "HOME", "HF_HOME", "HF_HUB_CACHE"] {
        assert!(names.contains(given), "{given} {names:?}");
    }
    // What `sh` itself adds is not Harold's.
    let sh = ["PWD", "SHLVL", "_", "OLDPWD"];
    let extra: Vec<&&str> = names
        .iter()
        .filter(|name| {
            !["PATH", "HOME", "HF_HOME", "HF_HUB_CACHE"].contains(name) && !sh.contains(name)
        })
        .collect();
    assert!(extra.is_empty(), "{extra:?}");
    assert!(!env.contains("token-value"));
}

#[tokio::test(flavor = "current_thread")]
async fn the_script_is_written_for_the_user_only_in_a_private_folder() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let models = scratch.models(&scratch.fake_python(WORKS));
    // An old file with a wider mode is never reused.
    std::fs::create_dir_all(scratch.folder()).unwrap();
    let script = scratch.folder().join("voice_download.py");
    std::fs::write(&script, "old").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o666)).unwrap();
    std::fs::set_permissions(scratch.folder(), std::fs::Permissions::from_mode(0o755)).unwrap();

    models.start_up();
    until(&models, finished).await;

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&scratch.folder()), 0o700);
    assert_eq!(mode(&script), 0o600);
    assert_eq!(std::fs::read_to_string(&script).unwrap(), VOICE_DOWNLOAD_PY);
}

#[tokio::test(flavor = "current_thread")]
async fn the_screen_follows_the_bytes_in_the_blobs_folder_every_ten_seconds() {
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let blobs = repo_dir(&scratch.cache(), WHISPER).join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join("a"), vec![0u8; 300]).unwrap();
    std::fs::write(blobs.join("b.incomplete"), vec![0u8; 200]).unwrap();
    let pid = scratch.root.join("pid");
    let models = scratch.models(&scratch.fake_python(&format!(
        "echo '{{\"total\": 1000}}'; echo $$ > {}; exec sleep 100000",
        pid.display()
    )));
    models.start_up();
    while !pid.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::pause();
    tokio::time::advance(PROGRESS_EVERY + Duration::from_secs(1)).await;
    tokio::time::resume();

    let seen = until(&models, |s| matches!(s, Status::Downloading { .. })).await;
    assert_eq!(
        seen,
        Status::Downloading {
            model: WHISPER.into(),
            bytes: 500,
            total: 1000
        }
    );
    assert_eq!(seen.percent(), Some(50));
}

#[tokio::test(flavor = "current_thread")]
async fn a_download_past_its_limit_is_killed_with_its_children_and_fails() {
    let logs = CapturedLogs::start();
    let scratch = Scratch::new();
    scratch.both_checked();
    scratch.record(WHISPER, WHISPER_REVISION, 999);
    let pid = scratch.root.join("pid");
    let child = scratch.root.join("child-pid");
    let models = scratch.models(&scratch.fake_python(&format!(
        "echo $$ > {}; sleep 100000 & echo $! > {}; wait",
        pid.display(),
        child.display()
    )));
    models.start_up();
    while !child.exists() || std::fs::read_to_string(&child).unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let alive = |file: &Path| {
        let number = std::fs::read_to_string(file).unwrap();
        std::process::Command::new("/bin/kill")
            .args(["-0", number.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(&pid) && alive(&child));

    tokio::time::pause();
    tokio::time::advance(DOWNLOAD_LIMIT + Duration::from_secs(1)).await;
    tokio::time::resume();

    let Status::Failed(missing) = until(&models, finished).await else {
        panic!("the download timed out");
    };
    assert_eq!(missing.reason, "failed");
    // Give the killed group a moment to be reaped.
    for _ in 0..50 {
        if !alive(&pid) && !alive(&child) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(&pid), "the script is still running");
    assert!(!alive(&child), "the script's child is still running");
    assert!(logs.text().contains("speech model not downloaded"));
}

#[test]
fn the_download_script_never_sends_a_token_or_uses_another_endpoint() {
    let source = VOICE_DOWNLOAD_PY;
    let code: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    let code_text = code.join("\n");

    // Every call that reaches Hugging Face passes `token=False`.
    for call in ["HfApi(", "model_info(", "snapshot_download("] {
        let mut seen = 0;
        let mut rest = code_text.as_str();
        while let Some(at) = rest.find(call) {
            let after = &rest[at..];
            let end = after.find(')').expect("the call closes");
            // A call's arguments may hold a nested call; the first `)` is enough for these.
            let arguments = &after[..end];
            assert!(arguments.contains("token=False"), "{call} {arguments}");
            seen += 1;
            rest = &after[end..];
        }
        assert!(seen >= 1, "{call} is called");
    }

    // The library never reads a token or reports telemetry: set before it is imported.
    let set = code_text.find("HF_HUB_DISABLE_IMPLICIT_TOKEN").unwrap();
    let telemetry = code_text.find("HF_HUB_DISABLE_TELEMETRY").unwrap();
    let import = code_text.find("from huggingface_hub import").unwrap();
    assert!(set < import && telemetry < import);
    assert!(code_text.contains("os.environ[\"HF_HUB_DISABLE_IMPLICIT_TOKEN\"] = \"1\""));
    assert!(!code_text[..import].contains("import huggingface_hub"));

    // Never set.
    for forbidden in [
        "HF_ENDPOINT",
        "HF_HUB_ENABLE_HF_TRANSFER",
        "local_dir",
        "HF_TOKEN",
    ] {
        assert!(!source.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn the_download_script_imports_only_the_standard_library_and_huggingface_hub() {
    let allowed = [
        "errno",
        "hashlib",
        "json",
        "os",
        "re",
        "sys",
        "huggingface_hub",
    ];
    for line in VOICE_DOWNLOAD_PY.lines() {
        let mut words = line.split_whitespace();
        if !matches!(words.next(), Some("import" | "from")) {
            continue;
        }
        let module = words.next().unwrap();
        assert!(allowed.contains(&module), "unexpected import: {line}");
    }
}
