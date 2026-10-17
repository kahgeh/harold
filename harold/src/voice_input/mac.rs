//! The Mac's side of a call: the microphone through `ffmpeg`, the speakers through `afplay`,
//! and `caffeinate` to keep the display awake while the call lasts.

use super::worker::{DEFAULT_VOICE, ProcessWorker, WorkerCommand, WorkerError};
use super::{AudioSink, AudioSource, CallPorts};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::outbound::tts;
use crate::settings::{Settings, TtsSettings};
use crate::timed_prompts::TmuxPorts;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::oneshot;
use tracing::warn;

pub(crate) const FFMPEG: &str = "ffmpeg";
pub(crate) const CAFFEINATE: &str = "caffeinate";

/// `-d -w <pid>`: keep the display awake, and exit when the process `pid` (Harold) does,
/// even when Harold is killed without dropping its child.
pub(crate) fn caffeinate_args(pid: u32) -> Vec<String> {
    vec!["-d".to_string(), "-w".to_string(), pid.to_string()]
}

/// `-nostdin -hide_banner -loglevel error -f avfoundation -i :default -ac 1 -ar 16000 -f s16le -`
pub(crate) fn ffmpeg_args() -> Vec<&'static str> {
    vec![
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "avfoundation",
        "-i",
        ":default",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-f",
        "s16le",
        "-",
    ]
}

/// The microphone: `ffmpeg`'s raw 16 kHz mono output, read as it arrives.
pub(crate) struct MicSource {
    _child: Child,
    stdout: ChildStdout,
    /// An odd byte left over from the last read, the first half of the next sample.
    spare: Option<u8>,
}

impl MicSource {
    pub(crate) fn open() -> std::io::Result<Self> {
        Self::open_with(FFMPEG, &ffmpeg_args())
    }

    pub(crate) fn open_with(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        Ok(Self {
            _child: child,
            stdout,
            spare: None,
        })
    }
}

impl AudioSource for MicSource {
    async fn next_block(&mut self) -> Option<Vec<i16>> {
        let mut buf = [0u8; 8192];
        loop {
            // The carried byte stays in `spare` until the read completes, so a dropped
            // future loses nothing.
            let start = match self.spare {
                Some(byte) => {
                    buf[0] = byte;
                    1
                }
                None => 0,
            };
            let read = self.stdout.read(&mut buf[start..]).await.ok()?;
            self.spare = None;
            if read == 0 {
                return None;
            }
            let total = start + read;
            if total % 2 == 1 {
                self.spare = Some(buf[total - 1]);
            }
            let samples: Vec<i16> = buf[..total - total % 2]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            if !samples.is_empty() {
                return Some(samples);
            }
        }
    }
}

/// The speakers: plays each file with a program such as `afplay`.
pub(crate) struct MacSink {
    program: String,
    args: Vec<String>,
    /// How the running `play`, if any, is told to stop.
    stopper: Mutex<Stopper>,
}

/// The stop of the `play` started last, while it runs.
#[derive(Default)]
struct Stopper {
    /// Counts the plays started, so a play that ends clears only its own stop.
    generation: u64,
    stop: Option<oneshot::Sender<()>>,
}

impl MacSink {
    pub(crate) fn new() -> Self {
        Self::with_program("afplay", &[])
    }

    /// The file to play is passed as the last argument.
    pub(crate) fn with_program(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            stopper: Mutex::default(),
        }
    }

    fn stopper(&self) -> MutexGuard<'_, Stopper> {
        self.stopper.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs the player on `wav` until it ends or `stopped` fires.
    async fn run(&self, wav: &Path, stopped: oneshot::Receiver<()>) -> Result<(), String> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .arg(wav)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", self.program))?;
        tokio::select! {
            status = child.wait() => match status {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(format!("{} exited with {status}", self.program)),
                Err(error) => Err(format!("{} failed: {error}", self.program)),
            },
            _ = stopped => {
                let _ = child.kill().await;
                Ok(())
            }
        }
    }
}

impl AudioSink for MacSink {
    /// The stop is in place before the player starts, so a `stop` as it starts is not lost,
    /// and is cleared when the play ends.
    async fn play(&self, wav: &Path) -> Result<(), String> {
        let (stop, stopped) = oneshot::channel();
        let generation = {
            let mut stopper = self.stopper();
            stopper.generation += 1;
            stopper.stop = Some(stop);
            stopper.generation
        };
        let played = self.run(wav, stopped).await;
        let mut stopper = self.stopper();
        if stopper.generation == generation {
            stopper.stop = None;
        }
        played
    }

    fn stop(&self) {
        let stop = self.stopper().stop.take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
    }
}

/// `caffeinate -d -w <Harold's pid>`, held for a call (Decision 16); dropping it kills the
/// child, and the child exits by itself when Harold does.
pub(crate) struct KeepAwake {
    _child: Child,
}

impl KeepAwake {
    pub(crate) fn start() -> std::io::Result<Self> {
        Self::start_with(CAFFEINATE, &caffeinate_args(std::process::id()))
    }

    pub(crate) fn start_with<S: AsRef<OsStr>>(program: &str, args: &[S]) -> std::io::Result<Self> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self { _child: child })
    }

    #[cfg(test)]
    fn pid(&self) -> u32 {
        self._child.id().expect("child is running")
    }
}

/// The daemon's call ports: the Mac's microphone, speakers and display, the voice worker
/// `[voice_input]` names, the chief's tmux pane and the `[tts]` command.
pub(crate) struct MacPorts {
    panes: TmuxPorts,
    /// `None` without `[voice_input]` or its Whisper model; no call starts then.
    worker: Option<WorkerCommand>,
    chief_folder: Option<String>,
    tts: Arc<TtsSettings>,
    sink: Arc<MacSink>,
}

impl MacPorts {
    pub(crate) fn from_settings(settings: &Settings, snapshots: AgentSnapshotHub) -> Self {
        let tts = &settings.tts;
        Self {
            panes: TmuxPorts::new(snapshots, settings.agents.0.clone()),
            worker: settings
                .voice_input
                .as_ref()
                .and_then(|voice_input| WorkerCommand::from_settings(voice_input, tts)),
            chief_folder: settings.notify.away_messages_only_from.clone(),
            tts: Arc::new(TtsSettings {
                command: tts.command.clone(),
                voice: tts.voice.clone(),
                args: tts.args.clone(),
                fallback_command: tts.fallback_command.clone(),
                fallback_voice: tts.fallback_voice.clone(),
                fallback_args: tts.fallback_args.clone(),
            }),
            sink: Arc::new(MacSink::new()),
        }
    }

    /// Whether a call can start: `[voice_input]` is there and names its Whisper model.
    pub(crate) fn set_up(&self) -> bool {
        self.worker.is_some()
    }
}

/// The last component of `folder`, as a turn-end update names its agent.
fn last_component(folder: &str) -> String {
    Path::new(folder).file_name().map_or_else(
        || folder.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

impl CallPorts for MacPorts {
    type Source = MicSource;
    type Sink = MacSink;
    type Worker = ProcessWorker;
    type Panes = TmuxPorts;
    type Awake = KeepAwake;

    fn open_source(&self) -> Result<MicSource, String> {
        MicSource::open().map_err(|error| error.to_string())
    }

    fn sink(&self) -> Arc<MacSink> {
        Arc::clone(&self.sink)
    }

    fn start_worker(&self, call_dir: &Path) -> Result<ProcessWorker, WorkerError> {
        let command = self.worker.as_ref().ok_or(WorkerError::FailedToLoad)?;
        ProcessWorker::start(command, call_dir)
    }

    fn panes(&self) -> &TmuxPorts {
        &self.panes
    }

    fn chief_folder(&self) -> Option<String> {
        self.chief_folder.clone()
    }

    async fn speak_with_tts(&self, line: &str) {
        let tts = Arc::clone(&self.tts);
        let line = line.to_string();
        // A failed command is logged where it runs.
        let _ = tokio::task::spawn_blocking(move || {
            tts::speak_line_with(&tts, &line, tts::desk_line_stopper())
        })
        .await;
    }

    fn stop_desk_line(&self) {
        tts::desk_line_stopper().stop();
    }

    async fn turn_end_update(&self, prompt: &str) -> String {
        let prompt = prompt.to_string();
        let context = last_component(self.chief_folder.as_deref().unwrap_or_default());
        let update =
            tokio::task::spawn_blocking(move || tts::desk_update_line(&prompt, &context)).await;
        update.unwrap_or_else(|_| {
            warn!("turn-end update not made");
            "Work complete".to_string()
        })
    }

    fn calls_dir(&self) -> PathBuf {
        std::env::temp_dir().join("harold-voice-input")
    }

    fn phrases_dir(&self) -> PathBuf {
        std::env::temp_dir().join("harold-voice-phrases")
    }

    fn voice_name(&self) -> String {
        self.worker
            .as_ref()
            .map_or(DEFAULT_VOICE, |command| command.voice.as_str())
            .to_string()
    }

    fn voice_muted(&self) -> bool {
        crate::outbound::is_tts_muted()
    }

    fn keep_awake(&self) -> Result<KeepAwake, String> {
        KeepAwake::start().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::time::{sleep, timeout};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("harold-mac-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ffmpeg_reads_the_default_input_as_16_khz_mono() {
        assert_eq!(
            ffmpeg_args(),
            [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "avfoundation",
                "-i",
                ":default",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-f",
                "s16le",
                "-"
            ]
        );
    }

    #[tokio::test]
    async fn mic_source_reads_samples_from_the_programs_output() {
        let dir = scratch("mic");
        let file = dir.join("pcm");
        let samples: Vec<i16> = (0..500).map(|n| (n * 37 - 9000) as i16).collect();
        let mut bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        bytes.push(0x7f); // a dangling byte is never a sample
        assert_eq!(bytes.len(), 1_001);
        std::fs::write(&file, bytes).unwrap();

        let mut mic = MicSource::open_with("/bin/cat", &[file.to_str().unwrap()]).unwrap();
        let mut heard = Vec::new();
        while let Some(block) = mic.next_block().await {
            heard.extend(block);
        }
        assert_eq!(heard, samples);
        assert_eq!(mic.next_block().await, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn sink_play_resolves_when_the_program_ends() {
        let sink = MacSink::with_program("/usr/bin/true", &[]);
        assert_eq!(sink.play(Path::new("/nowhere.wav")).await, Ok(()));
    }

    #[tokio::test]
    async fn sink_stop_ends_play_at_once() {
        let dir = scratch("stop");
        let pid_file = dir.join("pid");
        let wav = dir.join("endless");
        std::fs::write(&wav, "x").unwrap();
        let fake = script(&dir, "echo $$ > \"$1\"\nexec /usr/bin/tail -f \"$2\"");
        let sink = Arc::new(MacSink::with_program(
            fake.to_str().unwrap(),
            &[pid_file.to_str().unwrap()],
        ));
        let playing = tokio::spawn({
            let sink = Arc::clone(&sink);
            async move { sink.play(&wav).await }
        });
        wait_for_pid(&pid_file).await;

        sink.stop();

        let played = timeout(Duration::from_secs(1), playing).await;
        assert_eq!(played.expect("play ended after stop").unwrap(), Ok(()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_play_that_ends_leaves_no_stopper_behind() {
        let sink = MacSink::with_program("/usr/bin/true", &[]);
        assert_eq!(sink.play(Path::new("/nowhere.wav")).await, Ok(()));
        assert!(sink.stopper().stop.is_none());

        let missing = MacSink::with_program("/nowhere/player", &[]);
        assert!(missing.play(Path::new("/nowhere.wav")).await.is_err());
        assert!(missing.stopper().stop.is_none());
    }

    #[tokio::test]
    async fn a_stop_before_play_does_not_stop_the_next_play() {
        let dir = scratch("early-stop");
        let file = dir.join("endless");
        std::fs::write(&file, "x").unwrap();
        let sink = MacSink::with_program("/usr/bin/tail", &["-f"]);
        sink.stop();
        let played = timeout(Duration::from_millis(500), sink.play(&file)).await;
        assert!(played.is_err(), "play was still running");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mac_ports_without_voice_input_are_not_set_up() {
        crate::settings::init_settings_for_test();
        let settings = crate::settings::get_settings();
        assert!(settings.voice_input.is_none());
        let snapshots = AgentSnapshotHub::new(crate::agent::domain::AgentSnapshot {
            tts_muted: false,
            messaging_paused: false,
            through_event_version: crate::store::EventVersion::start(),
            server_time_ms: 1_000,
            monitor_health: Vec::new(),
            panes: Vec::new(),
        });

        let ports = MacPorts::from_settings(settings, snapshots);

        assert!(!ports.set_up());
        assert_eq!(ports.voice_name(), DEFAULT_VOICE);
        let temp = std::env::temp_dir();
        assert_eq!(ports.calls_dir(), temp.join("harold-voice-input"));
        assert_eq!(ports.phrases_dir(), temp.join("harold-voice-phrases"));
        assert_eq!(
            ports.start_worker(&temp).err(),
            Some(WorkerError::FailedToLoad)
        );
    }

    #[test]
    fn a_turn_end_update_names_the_chief_by_its_folders_last_component() {
        assert_eq!(last_component("/Users/me/chief-of-pods"), "chief-of-pods");
        assert_eq!(last_component("/Users/me/chief-of-pods/"), "chief-of-pods");
        assert_eq!(last_component(""), "");
    }

    #[test]
    fn the_real_programs_are_ffmpeg_afplay_and_caffeinate() {
        assert_eq!(FFMPEG, "ffmpeg");
        let sink = MacSink::new();
        assert_eq!((sink.program.as_str(), sink.args.len()), ("afplay", 0));
        assert_eq!(CAFFEINATE, "caffeinate");
        assert_eq!(caffeinate_args(4321), ["-d", "-w", "4321"]);
    }

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn an_odd_byte_waits_for_the_next_read() {
        let dir = scratch("odd");
        let fake = script(&dir, "printf '\\001'; sleep 0.2; printf '\\000\\002\\000'");
        let mut mic = MicSource::open_with(fake.to_str().unwrap(), &[]).unwrap();
        let mut heard = Vec::new();
        while let Some(block) = mic.next_block().await {
            heard.extend(block);
        }
        assert_eq!(heard, [1, 2]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_read_loses_no_byte() {
        let dir = scratch("cancel");
        let fake = script(&dir, "printf '\\001'; sleep 0.4; printf '\\000\\002\\000'");
        let mut mic = MicSource::open_with(fake.to_str().unwrap(), &[]).unwrap();
        let early = timeout(Duration::from_millis(150), mic.next_block()).await;
        assert!(early.is_err(), "the rest had not arrived yet");
        assert_eq!(mic.next_block().await, Some(vec![1, 2]));
        assert_eq!(mic.next_block().await, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn sink_play_reports_a_player_that_failed() {
        let sink = MacSink::with_program("/usr/bin/false", &[]);
        let result = sink.play(Path::new("/nowhere.wav")).await;
        assert!(result.is_err(), "{result:?}");
    }

    #[tokio::test]
    async fn sink_stop_kills_the_player() {
        let dir = scratch("kill");
        let pid_file = dir.join("pid");
        let wav = dir.join("endless");
        std::fs::write(&wav, "x").unwrap();
        let fake = script(&dir, "echo $$ > \"$1\"\nexec /usr/bin/tail -f \"$2\"");
        let sink = MacSink::with_program(fake.to_str().unwrap(), &[pid_file.to_str().unwrap()]);
        let stopping = async {
            let pid = wait_for_pid(&pid_file).await;
            assert!(running(pid));
            sink.stop();
            pid
        };
        let (played, pid) = tokio::join!(sink.play(&wav), stopping);
        assert_eq!(played, Ok(()));
        let deadline = Instant::now() + Duration::from_secs(1);
        while running(pid) && Instant::now() < deadline {
            sleep(Duration::from_millis(20)).await;
        }
        assert!(!running(pid), "player still running after stop");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The player's pid, once the script has written a whole line: the player is running.
    async fn wait_for_pid(pid_file: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let written = std::fs::read_to_string(pid_file).unwrap_or_default();
            if let Some(pid) = written.strip_suffix('\n').and_then(|pid| pid.parse().ok()) {
                return pid;
            }
            assert!(Instant::now() < deadline, "the player never wrote its pid");
            sleep(Duration::from_millis(20)).await;
        }
    }

    fn running(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn keep_awake_runs_until_dropped() {
        let dir = scratch("awake");
        let file = dir.join("endless");
        std::fs::write(&file, "x").unwrap();
        let awake =
            KeepAwake::start_with("/usr/bin/tail", &["-f", file.to_str().unwrap()]).unwrap();
        let pid = awake.pid();
        assert!(running(pid));
        drop(awake);
        let deadline = Instant::now() + Duration::from_secs(1);
        while running(pid) && Instant::now() < deadline {
            sleep(Duration::from_millis(20)).await;
        }
        assert!(!running(pid), "child still running after drop");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
