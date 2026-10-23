//! The client of the voice worker: a Python program that keeps Whisper and Kokoro loaded
//! for the length of a call and answers one JSON line per request.

use crate::settings::{TtsSettings, VoiceInputSettings};
use serde_json::{Value, json};
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// The worker's program, written into the call's folder when the call starts.
pub(crate) const VOICE_WORKER_PY: &str = include_str!("../voice_worker.py");
/// How long the worker has to answer one request.
pub(crate) const REQUEST_LIMIT: Duration = Duration::from_secs(30);
/// How long the worker has to load both models.
pub(crate) const LOAD_LIMIT: Duration = Duration::from_secs(120);
/// How long `close` waits for the worker to exit before it kills it.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);

const DEFAULT_TTS_MODEL: &str = "mlx-community/Kokoro-82M-bf16";
pub(super) const DEFAULT_VOICE: &str = "af_heart";
const DEFAULT_LANG_CODE: &str = "a";

/// What Whisper heard in one turn.
pub(crate) struct Transcript {
    pub text: String,
    pub no_speech_prob: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerError {
    /// The worker's output closed or the worker died.
    Exited,
    /// The worker did not answer within `REQUEST_LIMIT`; it has been killed.
    TimedOut,
    /// The worker could not load its models, or did not within `LOAD_LIMIT`.
    FailedToLoad,
    /// The worker answered that this one request failed.
    Failed,
}

impl WorkerError {
    /// The `reason` of `voice worker exited`; `None` for `Failed`, which leaves the worker running.
    pub(crate) fn exit_reason(self) -> Option<&'static str> {
        match self {
            WorkerError::Exited => Some("exited"),
            WorkerError::TimedOut => Some("timed_out"),
            WorkerError::FailedToLoad => Some("failed_to_load"),
            WorkerError::Failed => None,
        }
    }
}

/// Whether a worker has loaded, and whether it is still there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Loading,
    Ready,
    /// The worker has gone: it could not load, exited, or was killed for not answering.
    Gone(WorkerError),
}

/// One call's worker. Requests are answered one at a time. Requests are not cancel-safe:
/// a caller that drops a request future must drop (or close) the worker.
pub(crate) trait VoiceWorker: Send + 'static {
    /// The worker's health as it changes, between requests as well as during them.
    /// Waiting for a change is cancel-safe.
    fn health(&self) -> watch::Receiver<Health>;
    fn transcribe(
        &mut self,
        wav: &Path,
    ) -> impl Future<Output = Result<Transcript, WorkerError>> + Send;
    fn say(
        &mut self,
        text: &str,
        wav: &Path,
    ) -> impl Future<Output = Result<(), WorkerError>> + Send;
    /// Closes the worker's input and waits up to 5 s for it to exit, then kills it.
    fn close(self) -> impl Future<Output = ()> + Send;
}

/// What it takes to start a worker.
pub(crate) struct WorkerCommand {
    pub python: String,
    pub stt_model: String,
    pub tts_model: String,
    pub voice: String,
    pub lang_code: String,
}

impl WorkerCommand {
    /// The worker's command from the settings; `None` without a Whisper model.
    pub(crate) fn from_settings(
        voice_input: &VoiceInputSettings,
        tts: &TtsSettings,
    ) -> Option<Self> {
        let args = tts.args.as_deref().unwrap_or_default();
        let option = |name: &str, default: &str| {
            let value = args
                .iter()
                .position(|arg| arg == name)
                .and_then(|at| args.get(at + 1));
            value.map_or(default, String::as_str).to_string()
        };
        Some(Self {
            python: voice_input.command.clone(),
            stt_model: voice_input.whisper_model()?.to_string(),
            tts_model: option("--model", DEFAULT_TTS_MODEL),
            voice: option("--voice", DEFAULT_VOICE),
            lang_code: option("--lang_code", DEFAULT_LANG_CODE),
        })
    }
}

/// The worker as a child process, spoken to over JSON lines. A reader task owns the child
/// and its output: it waits for the ready line, passes each later line on as an answer, and
/// reports the worker's health as it changes.
pub(crate) struct ProcessWorker {
    stdin: Option<ChildStdin>,
    answers: mpsc::UnboundedReceiver<String>,
    health: watch::Receiver<Health>,
    /// Tells the reader to kill the child, and why; dropping it kills the child too.
    kill: Option<oneshot::Sender<WorkerError>>,
    reader: JoinHandle<()>,
}

impl ProcessWorker {
    /// Writes the script into `call_dir` (mode 0600) and starts it. Loading goes on in the
    /// background, and must end with the ready line within `LOAD_LIMIT`.
    pub(crate) fn start(command: &WorkerCommand, call_dir: &Path) -> Result<Self, WorkerError> {
        let script = call_dir.join("voice_worker.py");
        match std::fs::remove_file(&script) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(WorkerError::FailedToLoad),
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&script)
            .and_then(|mut file| file.write_all(VOICE_WORKER_PY.as_bytes()))
            .map_err(|_| WorkerError::FailedToLoad)?;
        let mut child = Command::new(&command.python)
            .arg("-I")
            .arg(&script)
            .args(["--stt-model", &command.stt_model])
            .args(["--tts-model", &command.tts_model])
            .args(["--voice", &command.voice])
            .args(["--lang-code", &command.lang_code])
            .current_dir(call_dir)
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| WorkerError::FailedToLoad)?;
        let stdin = child.stdin.take().ok_or(WorkerError::FailedToLoad)?;
        let stdout = child.stdout.take().ok_or(WorkerError::FailedToLoad)?;
        let lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stdout));
        let (health_tx, health) = watch::channel(Health::Loading);
        let (answers_tx, answers) = mpsc::unbounded_channel();
        let (kill, killed) = oneshot::channel();
        let reader = tokio::spawn(read_output(child, lines, health_tx, answers_tx, killed));
        Ok(Self {
            stdin: Some(stdin),
            answers,
            health,
            kill: Some(kill),
            reader,
        })
    }

    fn kill(&mut self, why: WorkerError) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(why);
        }
    }

    /// Why the worker has gone, once its output has ended.
    fn gone_reason(&self) -> WorkerError {
        match *self.health.borrow() {
            Health::Gone(error) => error,
            Health::Loading | Health::Ready => WorkerError::Exited,
        }
    }

    /// Waits for the worker's ready line.
    async fn wait_until_ready(&self) -> Result<(), WorkerError> {
        let mut health = self.health.clone();
        let health = health
            .wait_for(|health| *health != Health::Loading)
            .await
            .map(|health| *health);
        match health {
            Ok(Health::Ready) => Ok(()),
            Ok(Health::Gone(error)) => Err(error),
            Ok(Health::Loading) | Err(_) => Err(WorkerError::FailedToLoad),
        }
    }

    /// Sends one request and returns the worker's answer, which has `"ok": true`.
    async fn request(&mut self, request: Value) -> Result<Value, WorkerError> {
        self.wait_until_ready().await?;
        let mut line = request.to_string();
        line.push('\n');
        let stdin = self.stdin.as_mut().ok_or(WorkerError::Exited)?;
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return Err(self.gone_reason());
        }
        let line = match timeout(REQUEST_LIMIT, self.answers.recv()).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(self.gone_reason()),
            Err(_) => {
                self.kill(WorkerError::TimedOut);
                return Err(WorkerError::TimedOut);
            }
        };
        // A line that is not an answer would put every later answer out of step.
        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
            self.kill(WorkerError::Exited);
            return Err(WorkerError::Exited);
        };
        if reply["ok"].as_bool() != Some(true) {
            return Err(WorkerError::Failed);
        }
        Ok(reply)
    }
}

/// The reader task: the ready line within `LOAD_LIMIT`, then each answer line, until the
/// output ends or the worker is killed. The health it last reports is why the worker went.
/// Before the ready line, a line that is not JSON is a library's stray print and is skipped.
async fn read_output(
    mut child: Child,
    mut lines: Lines<BufReader<ChildStdout>>,
    health: watch::Sender<Health>,
    answers: mpsc::UnboundedSender<String>,
    mut killed: oneshot::Receiver<WorkerError>,
) {
    let mut kill_asked = false;
    let loaded = tokio::select! {
        ready = timeout(LOAD_LIMIT, wait_for_ready(&mut lines)) => match ready {
            Ok(true) => None,
            // A JSON line that is not the ready line, an output that ended or failed, or no
            // ready line in time.
            Ok(false) | Err(_) => Some(WorkerError::FailedToLoad),
        },
        why = &mut killed => {
            kill_asked = true;
            Some(why.unwrap_or(WorkerError::Exited))
        }
    };
    let gone = match loaded {
        Some(error) => error,
        None => {
            health.send_replace(Health::Ready);
            loop {
                tokio::select! {
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => {
                            let _ = answers.send(line);
                        }
                        Ok(None) | Err(_) => break WorkerError::Exited,
                    },
                    why = &mut killed => {
                        kill_asked = true;
                        break why.unwrap_or(WorkerError::Exited);
                    }
                }
            }
        }
    };
    health.send_replace(Health::Gone(gone));
    drop(answers);
    if gone == WorkerError::FailedToLoad || kill_asked {
        let _ = child.kill().await;
        return;
    }
    // The output ended: the worker is exiting. A kill asked for meanwhile ends the wait.
    tokio::select! {
        _ = child.wait() => {}
        _ = &mut killed => {
            let _ = child.kill().await;
        }
    }
}

/// Reads up to the first JSON line, and reports whether it is the ready line. A cancelled
/// wait (a kill, or the load limit) fails the load, so it need not be cancel-safe.
async fn wait_for_ready(lines: &mut Lines<BufReader<ChildStdout>>) -> bool {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(reply) = serde_json::from_str::<Value>(&line) {
            return reply["ready"].as_bool() == Some(true);
        }
    }
    false
}

impl VoiceWorker for ProcessWorker {
    fn health(&self) -> watch::Receiver<Health> {
        self.health.clone()
    }

    async fn transcribe(&mut self, wav: &Path) -> Result<Transcript, WorkerError> {
        let reply = self
            .request(json!({"op": "transcribe", "wav": wav.to_string_lossy()}))
            .await?;
        let text = reply["text"].as_str().ok_or(WorkerError::Failed)?;
        Ok(Transcript {
            text: text.to_string(),
            no_speech_prob: reply["no_speech_prob"].as_f64().unwrap_or(1.0) as f32,
        })
    }

    async fn say(&mut self, text: &str, wav: &Path) -> Result<(), WorkerError> {
        self.request(json!({"op": "say", "text": text, "wav": wav.to_string_lossy()}))
            .await
            .map(|_| ())
    }

    async fn close(mut self) {
        drop(self.stdin.take());
        if timeout(CLOSE_LIMIT, &mut self.reader).await.is_err() {
            self.kill(WorkerError::Exited);
            let _ = (&mut self.reader).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{TtsSettings, VoiceInputSettings};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn temp_folder() -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("harold-voice-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(folder.join("call")).unwrap();
        folder
    }

    /// Writes a fake worker that appends each input line to `log`, prints `ready`, then
    /// answers by `body`, and returns the command that runs it and the log's path.
    fn fake(folder: &Path, ready: &str, body: &str) -> (WorkerCommand, PathBuf) {
        let log = folder.join("log.txt");
        let script = folder.join("fake-worker");
        let text = format!(
            "#!/bin/sh\n{ready}\nwhile IFS= read -r line; do\necho \"$line\" >> {log}\n{body}\ndone\necho closed >> {log}\n",
            log = log.display()
        );
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = WorkerCommand {
            python: script.display().to_string(),
            stt_model: "stt".into(),
            tts_model: "tts".into(),
            voice: "af_heart".into(),
            lang_code: "a".into(),
        };
        (command, log)
    }

    const READY: &str = r#"echo '{"ready":true}'"#;
    const ANSWERS: &str = r#"case "$line" in
*'"op":"transcribe"'*) echo '{"ok":true,"text":"hello","no_speech_prob":0.1}' ;;
*'"op":"say"'*) echo '{"ok":true}' ;;
esac"#;

    #[tokio::test]
    async fn answers_transcribe_and_say_after_ready() {
        let folder = temp_folder();
        let (command, log) = fake(&folder, READY, ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let heard = worker.transcribe(Path::new("/x/turn.wav")).await.unwrap();
        assert_eq!(heard.text, "hello");
        assert!((heard.no_speech_prob - 0.1).abs() < 1e-6);
        worker
            .say("Hi there", Path::new("/x/say.wav"))
            .await
            .unwrap();
        worker.close().await;
        let logged = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 3, "{logged}");
        assert!(lines[0].contains(r#""op":"transcribe""#));
        assert!(lines[1].contains(r#""op":"say""#));
        assert_eq!(lines[2], "closed");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn close_shuts_its_input() {
        let folder = temp_folder();
        let (command, log) = fake(&folder, READY, ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        worker.say("Hi", Path::new("/x/say.wav")).await.unwrap();
        worker.close().await;
        assert!(std::fs::read_to_string(&log).unwrap().ends_with("closed\n"));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// The first health after `Loading`, or after `Ready` when `past_ready`.
    async fn health_after(worker: &ProcessWorker, past_ready: bool) -> Health {
        let mut health = worker.health();
        let seen = timeout(
            Duration::from_secs(5),
            health.wait_for(|health| {
                *health != Health::Loading && !(past_ready && *health == Health::Ready)
            }),
        )
        .await
        .expect("the health changes in time")
        .expect("the reader reports before it goes");
        *seen
    }

    #[tokio::test]
    async fn health_turns_ready_after_the_ready_line() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, READY, ANSWERS);
        let worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        assert_eq!(health_after(&worker, false).await, Health::Ready);
        worker.close().await;
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn an_exit_before_ready_is_seen_with_no_request() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, "exit 1", ANSWERS);
        let worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        assert_eq!(
            health_after(&worker, false).await,
            Health::Gone(WorkerError::FailedToLoad)
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn an_exit_after_ready_is_seen_with_no_request() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, &format!("{READY}\nexit 0"), ANSWERS);
        let worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        assert_eq!(
            health_after(&worker, true).await,
            Health::Gone(WorkerError::Exited)
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn a_worker_that_exits_gives_exited() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, &format!("{READY}\nexit 0"), ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.transcribe(Path::new("/x/turn.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::Exited));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn ready_false_gives_failed_to_load() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, r#"echo '{"ready":false}'; exit 0"#, ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.say("Hi", Path::new("/x/say.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::FailedToLoad));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn a_stray_print_before_ready_is_skipped() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, &format!("echo hello\necho\n{READY}"), ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let heard = worker.transcribe(Path::new("/x/turn.wav")).await.unwrap();
        assert_eq!(heard.text, "hello");
        worker.close().await;
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn a_json_line_before_ready_that_is_not_ready_fails_the_load() {
        let folder = temp_folder();
        let ready = format!("echo hello\necho '{{\"progress\":1}}'\n{READY}");
        let (command, _) = fake(&folder, &ready, ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.say("Hi", Path::new("/x/say.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::FailedToLoad));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn an_exit_before_ready_gives_failed_to_load() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, "exit 1", ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.say("Hi", Path::new("/x/say.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::FailedToLoad));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn ok_false_gives_failed_and_keeps_the_worker() {
        let folder = temp_folder();
        let body = r#"case "$line" in
*'"op":"transcribe"'*) echo '{"ok":false}' ;;
*) echo '{"ok":true}' ;;
esac"#;
        let (command, _) = fake(&folder, READY, body);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.transcribe(Path::new("/x/turn.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::Failed));
        assert_eq!(WorkerError::Failed.exit_reason(), None);
        assert!(worker.say("Hi", Path::new("/x/say.wav")).await.is_ok());
        worker.close().await;
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn exit_reasons_are_the_logged_words() {
        assert_eq!(WorkerError::Exited.exit_reason(), Some("exited"));
        assert_eq!(WorkerError::TimedOut.exit_reason(), Some("timed_out"));
        assert_eq!(
            WorkerError::FailedToLoad.exit_reason(),
            Some("failed_to_load")
        );
    }

    #[tokio::test]
    async fn the_script_is_written_for_the_user_only() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, READY, ANSWERS);
        let script = folder.join("call").join("voice_worker.py");
        // An old file with a wider mode, and a symlink, are never reused.
        std::fs::write(&script, "old").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o666)).unwrap();
        for _ in 0..2 {
            let worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
            let mode = std::fs::metadata(&script).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert_eq!(std::fs::read_to_string(&script).unwrap(), VOICE_WORKER_PY);
            worker.close().await;
        }
        std::fs::remove_file(&script).unwrap();
        let elsewhere = folder.join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, &script).unwrap();
        let worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        assert!(!elsewhere.exists());
        assert_eq!(std::fs::read_to_string(&script).unwrap(), VOICE_WORKER_PY);
        worker.close().await;
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[tokio::test]
    async fn a_line_that_is_not_an_answer_gives_exited() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, READY, "echo garbage");
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.say("Hi", Path::new("/x/say.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::Exited));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    fn tts(args: &[&str]) -> TtsSettings {
        TtsSettings {
            model_revision: crate::settings::KOKORO_REVISION.to_string(),
            command: "python".into(),
            voice: None,
            args: Some(args.iter().map(|arg| arg.to_string()).collect()),
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
        }
    }

    fn voice_input(args: &[&str]) -> VoiceInputSettings {
        VoiceInputSettings {
            model_revision: crate::settings::WHISPER_REVISION.to_string(),
            keep_worker_minutes: 10,
            command: "/venv/bin/python".into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    #[test]
    fn kokoro_options_come_from_tts_args() {
        let tts = tts(&[
            "-m",
            "mlx_audio.tts.generate",
            "--voice",
            "bf_emma",
            "--play",
            "--text",
        ]);
        let input = voice_input(&["-m", "mlx_audio.stt.generate", "--model", "whisper-x"]);
        let command = WorkerCommand::from_settings(&input, &tts).unwrap();
        assert_eq!(command.python, "/venv/bin/python");
        assert_eq!(command.stt_model, "whisper-x");
        assert_eq!(command.voice, "bf_emma");
        assert_eq!(command.tts_model, "mlx-community/Kokoro-82M-bf16");
        assert_eq!(command.lang_code, "a");

        let explicit = self::tts(&["--model", "m", "--lang_code", "b"]);
        let command = WorkerCommand::from_settings(&input, &explicit).unwrap();
        assert_eq!(command.tts_model, "m");
        assert_eq!(command.lang_code, "b");
        assert_eq!(command.voice, "af_heart");

        assert!(WorkerCommand::from_settings(&voice_input(&["-m", "x"]), &tts).is_none());
    }

    #[test]
    fn the_worker_imports_only_the_installed_packages() {
        let allowed = [
            "json",
            "sys",
            "traceback",
            "numpy",
            "soundfile",
            "mlx_audio.stt.utils",
            "mlx_audio.tts.utils",
        ];
        let mut seen = 0;
        for line in VOICE_WORKER_PY.lines() {
            let mut words = line.split_whitespace();
            if !matches!(words.next(), Some("import" | "from")) {
                continue;
            }
            let module = words.next().unwrap();
            assert!(allowed.contains(&module), "unexpected import: {line}");
            seen += 1;
        }
        assert_eq!(seen, allowed.len());
    }
}
