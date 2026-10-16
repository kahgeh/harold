//! The Mac's side of a call: the microphone through `ffmpeg`, the speakers through `afplay`,
//! and `caffeinate` to keep the display awake while the call lasts.

use super::{AudioSink, AudioSource};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::oneshot;

pub(crate) const FFMPEG: &str = "ffmpeg";
pub(crate) const CAFFEINATE: &str = "caffeinate";

/// `-d`: keep the display awake.
pub(crate) fn caffeinate_args() -> Vec<&'static str> {
    vec!["-d"]
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
    stopper: Mutex<Option<oneshot::Sender<()>>>,
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
            stopper: Mutex::new(None),
        }
    }
}

impl AudioSink for MacSink {
    async fn play(&self, wav: &Path) -> Result<(), String> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .arg(wav)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", self.program))?;
        let (stop, stopped) = oneshot::channel();
        *self.stopper.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop);
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

    fn stop(&self) {
        let stop = self
            .stopper
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
    }
}

/// `caffeinate -d`, held for a call (Decision 16); dropping it kills the child.
pub(crate) struct KeepAwake {
    child: Child,
}

impl KeepAwake {
    pub(crate) fn start() -> std::io::Result<Self> {
        Self::start_with(CAFFEINATE, &caffeinate_args())
    }

    pub(crate) fn start_with(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self { child })
    }

    #[cfg(test)]
    fn pid(&self) -> u32 {
        self.child.id().expect("child is running")
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
        let file = dir.join("endless");
        std::fs::write(&file, "x").unwrap();
        let sink = MacSink::with_program("/usr/bin/tail", &["-f"]);
        let stopping = async {
            sleep(Duration::from_millis(100)).await;
            sink.stop();
        };
        let (played, ()) =
            tokio::join!(timeout(Duration::from_secs(1), sink.play(&file)), stopping);
        assert_eq!(played.expect("play ended after stop"), Ok(()));
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn the_real_programs_are_ffmpeg_afplay_and_caffeinate() {
        assert_eq!(FFMPEG, "ffmpeg");
        let sink = MacSink::new();
        assert_eq!((sink.program.as_str(), sink.args.len()), ("afplay", 0));
        assert_eq!(CAFFEINATE, "caffeinate");
        assert_eq!(caffeinate_args(), ["-d"]);
        // These keep the not-yet-wired constructors referenced in test builds; never called,
        // as they would start the real programs.
        let _: fn() -> std::io::Result<MicSource> = MicSource::open;
        let _: fn() -> std::io::Result<KeepAwake> = KeepAwake::start;
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
            sleep(Duration::from_millis(300)).await;
            let pid: u32 = std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
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
