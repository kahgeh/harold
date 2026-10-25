//! The supervisor on a fake launcher, with paused time, and the process session on a fake
//! worker: a shell script that speaks the worker's JSON lines.

use super::*;
use crate::tts::test_support::*;
use std::os::unix::fs::PermissionsExt;

fn supervisor() -> (Arc<Supervisor>, Arc<Script>) {
    let script = Arc::new(Script::default());
    let supervisor = Supervisor::new(
        Arc::new(FakeLauncher(Arc::clone(&script))),
        Duration::from_secs(10 * 60),
    );
    (supervisor, script)
}

async fn say(supervisor: &Arc<Supervisor>, load: &Load) -> Result<Speaking, Refused> {
    supervisor
        .speak(load, "Hello there.".into(), "af_heart".into())
        .await
}

/// Reads the rest of a line: the audio pieces and how the line ended.
async fn finish(mut speaking: Speaking) -> (Vec<Chunk>, Result<Done, Failure>) {
    let mut pieces = vec![speaking.first.clone()];
    while let Some(chunk) = speaking.chunks.recv().await {
        pieces.push(chunk);
    }
    (pieces, speaking.end.await.unwrap())
}

/// Reads a line to its end, which must have worked.
async fn drain(speaking: Speaking) {
    let (_, done) = finish(speaking).await;
    done.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_worker_starts_on_the_first_request_and_streams_the_line() {
    let (supervisor, script) = supervisor();
    assert_eq!(script.launches.load(Ordering::SeqCst), 0);

    let speaking = say(&supervisor, &mlx("m")).await.ok().unwrap();
    assert!(speaking.loaded);
    assert_eq!(speaking.ready, Some(ready()));
    let (pieces, done) = finish(speaking).await;

    assert_eq!(pieces.len(), 3);
    assert!(pieces.iter().all(|piece| piece.sample_rate == 24_000));
    assert_eq!(done.unwrap().device, Device::AppleGpu);
    assert_eq!(script.launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_second_request_uses_the_worker_that_is_loaded() {
    let (supervisor, script) = supervisor();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;

    let second = say(&supervisor, &mlx("m")).await.ok().unwrap();

    assert!(!second.loaded);
    assert_eq!(second.ready, None);
    drain(second).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn another_model_stops_the_worker_and_starts_one_for_it() {
    let (supervisor, script) = supervisor();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;

    let other = say(&supervisor, &mlx("other")).await.ok().unwrap();

    assert!(other.loaded);
    drain(other).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 2);
    assert_eq!(script.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn stop_if_holding_stops_only_a_worker_that_holds_the_model() {
    let (supervisor, script) = supervisor();

    supervisor.stop_if_holding("m").await;
    assert_eq!(
        script.closed.load(Ordering::SeqCst),
        0,
        "no worker is running"
    );

    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;
    supervisor.stop_if_holding("other").await;
    assert_eq!(script.closed.load(Ordering::SeqCst), 0);
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 1);

    supervisor.stop_if_holding("m").await;
    assert_eq!(script.closed.load(Ordering::SeqCst), 1);
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn stop_if_holding_stops_a_worker_that_holds_the_onnx_model() {
    let (supervisor, script) = supervisor();
    drain(say(&supervisor, &onnx("m")).await.ok().unwrap()).await;

    supervisor.stop_if_holding("other").await;
    assert_eq!(script.closed.load(Ordering::SeqCst), 0);
    drain(say(&supervisor, &onnx("m")).await.ok().unwrap()).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 1);

    supervisor.stop_if_holding("m").await;
    assert_eq!(script.closed.load(Ordering::SeqCst), 1);
    assert!(!supervisor.is_loaded().await);
}

#[tokio::test(start_paused = true)]
async fn the_worker_stops_after_ten_minutes_idle_and_starts_on_the_next_request() {
    let (supervisor, script) = supervisor();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;

    tokio::time::advance(Duration::from_secs(9 * 60)).await;
    assert!(!supervisor.stop_if_idle().await);
    assert!(supervisor.is_loaded().await);

    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(supervisor.stop_if_idle().await);
    assert!(!supervisor.is_loaded().await);
    assert_eq!(script.closed.load(Ordering::SeqCst), 1);

    let again = say(&supervisor, &mlx("m")).await.ok().unwrap();
    assert!(again.loaded);
    drain(again).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_request_restarts_the_idle_time() {
    let (supervisor, _) = supervisor();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;
    tokio::time::advance(Duration::from_secs(9 * 60)).await;
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;

    tokio::time::advance(Duration::from_secs(9 * 60)).await;

    assert!(!supervisor.stop_if_idle().await);
}

#[tokio::test(start_paused = true)]
async fn the_worker_does_not_stop_in_a_call_of_more_than_ten_minutes() {
    let (supervisor, script) = supervisor();
    supervisor.call_opened();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;

    tokio::time::advance(Duration::from_secs(25 * 60)).await;
    assert!(!supervisor.stop_if_idle().await);
    assert_eq!(script.closed.load(Ordering::SeqCst), 0);

    // The idle time starts when the call ends.
    supervisor.call_ended();
    tokio::time::advance(Duration::from_secs(9 * 60)).await;
    assert!(!supervisor.stop_if_idle().await);
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(supervisor.stop_if_idle().await);
}

#[tokio::test(start_paused = true)]
async fn a_call_starts_the_worker_while_it_rings_so_the_first_line_does_not_wait() {
    let (supervisor, script) = supervisor();
    supervisor.call_opened();

    supervisor.warm(&mlx("m"), "af_heart").await.unwrap();
    assert_eq!(script.launches.load(Ordering::SeqCst), 1);

    let first_line = say(&supervisor, &mlx("m")).await.ok().unwrap();
    assert!(!first_line.loaded);
    drain(first_line).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_worker_that_exits_fails_the_request_and_the_next_request_starts_it_again() {
    let (supervisor, script) = supervisor();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;
    script.lines.lock().unwrap().push(Next::Exit);

    let failed = say(&supervisor, &mlx("m")).await.err().unwrap();
    assert_eq!(failed.failure, Failure::WorkerFailed);
    assert!(!supervisor.is_loaded().await);

    let again = say(&supervisor, &mlx("m")).await.ok().unwrap();
    assert!(again.loaded);
    drain(again).await;
    assert_eq!(script.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_worker_that_will_not_start_fails_the_request_with_its_reason() {
    let (supervisor, script) = supervisor();
    script
        .launch_failures
        .lock()
        .unwrap()
        .push(Failure::ModelMissing);

    let refused = say(&supervisor, &mlx("m")).await.err().unwrap();

    assert_eq!(refused.failure, Failure::ModelMissing);
    assert!(refused.loaded);
    assert!(!supervisor.is_loaded().await);
}

#[tokio::test(start_paused = true)]
async fn a_worker_with_no_audio_within_the_limit_times_out() {
    let (supervisor, script) = supervisor();
    script.lines.lock().unwrap().push(Next::Silent);

    let refused = say(&supervisor, &mlx("m")).await.err().unwrap();

    assert_eq!(refused.failure, Failure::TimedOut);
}

#[tokio::test(start_paused = true)]
async fn a_busy_worker_is_not_idle() {
    let (supervisor, script) = supervisor();
    script.lines.lock().unwrap().push(Next::Hold);
    let speaking = say(&supervisor, &mlx("m")).await.ok().unwrap();
    tokio::time::advance(Duration::from_secs(30 * 60)).await;

    // The line is not at its end yet: the worker is in use.
    assert!(!supervisor.stop_if_idle().await);
    assert_eq!(script.closed.load(Ordering::SeqCst), 0);

    script.release.notify_one();
    let (pieces, done) = finish(speaking).await;
    assert_eq!(pieces.len(), 2);
    assert!(done.is_ok());
    // The idle time starts when the line ends.
    assert!(!supervisor.stop_if_idle().await);
}

#[tokio::test(start_paused = true)]
async fn the_reaper_stops_an_idle_worker_and_stops_it_at_shutdown() {
    let (supervisor, script) = supervisor();
    drain(say(&supervisor, &mlx("m")).await.ok().unwrap()).await;
    let (shutdown, rx) = tokio::sync::watch::channel(());
    let reaper = tokio::spawn(Arc::clone(&supervisor).tend(rx));

    tokio::time::sleep(Duration::from_secs(10 * 60 + 10)).await;
    assert_eq!(script.closed.load(Ordering::SeqCst), 1);

    drop(shutdown);
    reaper.await.unwrap();
}

#[test]
fn this_build_holds_one_worker() {
    #[cfg(target_os = "macos")]
    {
        assert_eq!(WORKER_FILE, "tts_worker_mlx.py");
        assert!(WORKER_PY.contains("mlx_audio"));
        assert!(!WORKER_PY.contains("onnxruntime"));
        assert_eq!(WorkerKind::THIS_BUILD, WorkerKind::Mlx);
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert_eq!(WORKER_FILE, "tts_worker_onnx.py");
        assert!(WORKER_PY.contains("onnxruntime"));
        assert!(!WORKER_PY.contains("mlx_audio"));
        assert_eq!(WorkerKind::THIS_BUILD, WorkerKind::Onnx);
    }
}

#[test]
fn the_workers_import_only_what_they_need() {
    let mlx = include_str!("../tts_worker_mlx.py");
    let onnx = include_str!("../tts_worker_onnx.py");
    let imports = |program: &str| -> Vec<String> {
        program
            .lines()
            .filter(|line| !line.starts_with(' '))
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                matches!(words.next(), Some("import" | "from"))
                    .then(|| words.next().unwrap().into())
            })
            .collect()
    };
    let indented = |program: &str| -> Vec<String> {
        program
            .lines()
            .filter(|line| line.starts_with("    import ") || line.starts_with("    from "))
            .map(|line| line.split_whitespace().nth(1).unwrap().to_string())
            .collect()
    };

    let mut seen = imports(mlx);
    seen.extend(indented(mlx));
    seen.sort();
    assert_eq!(
        seen,
        [
            "json",
            "mlx_audio.tts.utils",
            "numpy",
            "pathlib",
            "sys",
            "time",
            "traceback"
        ]
    );
    let mut seen = imports(onnx);
    seen.extend(indented(onnx));
    seen.sort();
    assert_eq!(
        seen,
        [
            "glob",
            "json",
            "kokoro_onnx",
            "numpy",
            "onnxruntime",
            "os",
            "pathlib",
            "sys",
            "time",
            "traceback"
        ]
    );
}

/// A fake worker: an executable script run in place of the Python. It ignores its arguments.
struct FakePython {
    folder: PathBuf,
    program: PathBuf,
}

impl FakePython {
    fn new(body: &str) -> Self {
        let folder = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let program = folder.join("fake-python");
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { folder, program }
    }

    fn launcher(&self) -> ProcessLauncher {
        ProcessLauncher::new(
            self.program.display().to_string(),
            self.folder.join("models"),
        )
    }
}

impl Drop for FakePython {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

fn this_build_load() -> Load {
    if cfg!(target_os = "macos") {
        mlx("m")
    } else {
        Load::Onnx {
            model: "m".into(),
            model_file: "/x/m.onnx".into(),
            voices_file: "/x/v.bin".into(),
        }
    }
}

const READY_LINE: &str = r#"printf '%s\n' '{"ready": true, "load_ms": 42, "device": "gpu", "gpu_reason": null, "voices": ["af_heart", "am_adam"]}'"#;

/// Answers each request with two pieces of audio and a done line.
const ANSWER_LOOP: &str = r#"while read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id": *\([0-9]*\).*/\1/')
  printf '{"id": %s, "chunk": "01000200", "rate": 24000}\n' "$id"
  printf '{"id": %s, "chunk": "03000400", "rate": 24000}\n' "$id"
  printf '{"id": %s, "done": true, "device": "cpu", "gpu_reason": "gpu_out_of_memory"}\n' "$id"
done"#;

#[tokio::test]
async fn the_process_session_reads_the_ready_line_and_streams_the_audio() {
    let fake = FakePython::new(&format!("{READY_LINE}\n{ANSWER_LOOP}"));
    let launcher = fake.launcher();

    let mut session = launcher
        .launch(&this_build_load(), "af_heart")
        .await
        .ok()
        .unwrap();
    assert_eq!(session.ready().load_ms, 42);
    assert_eq!(session.ready().device, Device::Gpu);
    assert_eq!(session.ready().voices, ["af_heart", "am_adam"]);

    let (tx, mut rx) = mpsc::channel(8);
    let done = session.speak("Hello.", "af_heart", &tx).await.unwrap();
    drop(tx);
    let mut pcm = Vec::new();
    while let Some(chunk) = rx.recv().await {
        assert_eq!(chunk.sample_rate, 24_000);
        pcm.extend(chunk.pcm);
    }

    assert_eq!(pcm, [1, 0, 2, 0, 3, 0, 4, 0]);
    assert_eq!(done.device, Device::Cpu);
    assert_eq!(done.gpu_reason, Some("gpu_out_of_memory"));
    assert!(session.is_alive());
    session.close().await;
}

#[tokio::test]
async fn a_line_that_is_not_a_chunk_or_an_answer_fails_the_worker() {
    let fake = FakePython::new(&format!(
        "{READY_LINE}\nread -r line\nprintf 'not json\\n'\nsleep 5"
    ));
    let mut session = fake
        .launcher()
        .launch(&this_build_load(), "af_heart")
        .await
        .ok()
        .unwrap();
    let (tx, _rx) = mpsc::channel(8);

    let result = session.speak("Hello.", "af_heart", &tx).await;

    assert_eq!(result, Err(Failure::WorkerFailed));
}

#[tokio::test]
async fn a_worker_that_exits_mid_line_fails_the_line() {
    let fake = FakePython::new(&format!("{READY_LINE}\nread -r line\nexit 0"));
    let mut session = fake
        .launcher()
        .launch(&this_build_load(), "af_heart")
        .await
        .ok()
        .unwrap();
    let (tx, _rx) = mpsc::channel(8);

    let result = session.speak("Hello.", "af_heart", &tx).await;

    assert_eq!(result, Err(Failure::WorkerFailed));
}

#[tokio::test]
async fn an_error_answer_for_a_voice_the_worker_lacks_is_unknown_voice() {
    let fake = FakePython::new(&format!(
        "{READY_LINE}\nread -r line\nprintf '%s\\n' '{{\"id\": 1, \"error\": \"voice_missing\"}}'"
    ));
    let mut session = fake
        .launcher()
        .launch(&this_build_load(), "af_heart")
        .await
        .ok()
        .unwrap();
    let (tx, _rx) = mpsc::channel(8);

    let result = session.speak("Hello.", "nobody", &tx).await;

    assert_eq!(result, Err(Failure::UnknownVoice));
}

#[tokio::test]
async fn a_dropped_receiver_does_not_stop_the_line_from_being_read() {
    let fake = FakePython::new(&format!("{READY_LINE}\n{ANSWER_LOOP}"));
    let mut session = fake
        .launcher()
        .launch(&this_build_load(), "af_heart")
        .await
        .ok()
        .unwrap();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);

    let done = session.speak("Hello.", "af_heart", &tx).await;
    assert!(done.is_ok());

    // The next line is in step.
    let (tx, mut rx) = mpsc::channel(8);
    assert!(session.speak("Again.", "af_heart", &tx).await.is_ok());
    drop(tx);
    assert!(rx.recv().await.is_some());
    session.close().await;
}

#[tokio::test]
async fn a_stray_print_before_the_ready_line_is_skipped() {
    let fake = FakePython::new(&format!(
        "echo 'a library print'\n{READY_LINE}\n{ANSWER_LOOP}"
    ));

    let session = fake.launcher().launch(&this_build_load(), "af_heart").await;

    assert!(session.is_ok());
}

#[tokio::test]
async fn a_worker_that_says_it_is_not_ready_gives_its_cause() {
    for (cause, failure) in [
        ("model_missing", Failure::ModelMissing),
        ("voice_missing", Failure::UnknownVoice),
        ("import_failed", Failure::WorkerFailed),
        ("load_failed", Failure::WorkerFailed),
    ] {
        let fake = FakePython::new(&format!(
            r#"printf '%s\n' '{{"ready": false, "cause": "{cause}"}}'"#
        ));
        let result = fake.launcher().launch(&this_build_load(), "af_heart").await;
        assert_eq!(result.err(), Some(failure), "{cause}");
    }
}

#[tokio::test]
async fn a_worker_that_exits_before_ready_fails_the_start() {
    let fake = FakePython::new("exit 3");

    let result = fake.launcher().launch(&this_build_load(), "af_heart").await;

    assert_eq!(result.err(), Some(Failure::WorkerFailed));
}

#[tokio::test]
async fn the_worker_is_given_a_clean_environment_and_its_program_is_for_the_user_only() {
    let fake = FakePython::new(&format!(
        "env > \"$0.env\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\n{READY_LINE}\n{ANSWER_LOOP}"
    ));
    let launcher = fake.launcher();

    let session = launcher
        .launch(&this_build_load(), "af_heart")
        .await
        .ok()
        .unwrap();
    session.close().await;

    let env = std::fs::read_to_string(format!("{}.env", fake.program.display())).unwrap();
    assert!(env.contains("HF_HUB_OFFLINE=1"));
    // Nothing of the user's environment is passed on, so no token is.
    assert!(!env.contains("HF_TOKEN"));
    assert!(!env.lines().any(|line| line.starts_with("USER=")));
    let args = std::fs::read_to_string(format!("{}.args", fake.program.display())).unwrap();
    assert!(args.starts_with("-I\n"));
    assert!(args.contains(WORKER_FILE));
    assert!(args.contains("--voice\naf_heart"));
    let script = script_path(&fake.folder.join("models"));
    let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read_to_string(&script).unwrap(), WORKER_PY);
    let folder_mode = std::fs::metadata(fake.folder.join("models"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(folder_mode, 0o700);
}

#[tokio::test]
async fn a_load_for_the_other_platforms_worker_is_refused() {
    let fake = FakePython::new("exit 0");
    let other = if cfg!(target_os = "macos") {
        Load::Onnx {
            model: "m".into(),
            model_file: "/x/m.onnx".into(),
            voices_file: "/x/v.bin".into(),
        }
    } else {
        mlx("m")
    };

    let result = fake.launcher().launch(&other, "af_heart").await;

    assert_eq!(result.err(), Some(Failure::WorkerFailed));
}

#[test]
fn the_python_worker_tests_pass() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tts/test_tts_workers.py");

    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(script)
        .output()
        .expect("python3 runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
