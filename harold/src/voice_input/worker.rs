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
const DEFAULT_VOICE: &str = "af_heart";
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

/// One call's worker. Requests are answered one at a time.
pub(crate) trait VoiceWorker: Send + 'static {
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

/// The worker as a child process, spoken to over JSON lines.
pub(crate) struct ProcessWorker {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    ready: bool,
}

impl ProcessWorker {
    /// Writes the script into `call_dir` (mode 0600) and starts it. Loading goes on in the background:
    /// the first request waits for the ready line, up to `LOAD_LIMIT`.
    pub(crate) fn start(command: &WorkerCommand, call_dir: &Path) -> Result<Self, WorkerError> {
        let script = call_dir.join("voice_worker.py");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
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
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines: tokio::io::AsyncBufReadExt::lines(BufReader::new(stdout)),
            ready: false,
        })
    }

    /// Waits for the worker's ready line.
    async fn wait_until_ready(&mut self) -> Result<(), WorkerError> {
        if self.ready {
            return Ok(());
        }
        let line = match timeout(LOAD_LIMIT, self.lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(_) => return Err(WorkerError::FailedToLoad),
            Err(_) => {
                let _ = self.child.start_kill();
                return Err(WorkerError::FailedToLoad);
            }
        };
        let ready = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|reply| reply["ready"].as_bool());
        if ready != Some(true) {
            return Err(WorkerError::FailedToLoad);
        }
        self.ready = true;
        Ok(())
    }

    /// Sends one request and returns the worker's answer, which has `"ok": true`.
    async fn request(&mut self, request: Value) -> Result<Value, WorkerError> {
        self.wait_until_ready().await?;
        let stdin = self.stdin.as_mut().ok_or(WorkerError::Exited)?;
        let mut line = request.to_string();
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return Err(WorkerError::Exited);
        }
        let line = match timeout(REQUEST_LIMIT, self.lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(_) => return Err(WorkerError::Exited),
            Err(_) => {
                let _ = self.child.start_kill();
                return Err(WorkerError::TimedOut);
            }
        };
        let reply: Value = serde_json::from_str(&line).map_err(|_| WorkerError::Failed)?;
        if reply["ok"].as_bool() != Some(true) {
            return Err(WorkerError::Failed);
        }
        Ok(reply)
    }
}

impl VoiceWorker for ProcessWorker {
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
        if timeout(CLOSE_LIMIT, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

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
    }

    #[tokio::test]
    async fn close_shuts_its_input() {
        let folder = temp_folder();
        let (command, log) = fake(&folder, READY, ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        worker.say("Hi", Path::new("/x/say.wav")).await.unwrap();
        worker.close().await;
        assert!(std::fs::read_to_string(&log).unwrap().ends_with("closed\n"));
    }

    #[tokio::test]
    async fn a_worker_that_exits_gives_exited() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, &format!("{READY}\nexit 0"), ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.transcribe(Path::new("/x/turn.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::Exited));
    }

    #[tokio::test]
    async fn ready_false_gives_failed_to_load() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, r#"echo '{"ready":false}'; exit 0"#, ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.say("Hi", Path::new("/x/say.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::FailedToLoad));
    }

    #[tokio::test]
    async fn an_exit_before_ready_gives_failed_to_load() {
        let folder = temp_folder();
        let (command, _) = fake(&folder, "exit 1", ANSWERS);
        let mut worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let result = worker.say("Hi", Path::new("/x/say.wav")).await;
        assert_eq!(result.err(), Some(WorkerError::FailedToLoad));
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
        let worker = ProcessWorker::start(&command, &folder.join("call")).unwrap();
        let script = folder.join("call").join("voice_worker.py");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&script).unwrap(), VOICE_WORKER_PY);
        worker.close().await;
    }

    fn tts(args: &[&str]) -> TtsSettings {
        TtsSettings {
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
