//! Turns a spoken line into an AAC `.m4a` voice note with the model Harold speaks with at
//! the desk, or with `say` when the model is not configured or fails, and hands the file
//! to a send. Every program is run through a `Runner`, so tests run none of them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use tracing::warn;

use crate::outbound::tts::as_last_argument;
use crate::settings::TtsSettings;

/// The module `[tts]` runs when it speaks with the local model.
const MODEL_MODULE: &str = "mlx_audio.tts.generate";

/// How long the model, `afconvert` and `say` each get.
pub(crate) const MODEL_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs one program to its end. The daemon uses `SystemRunner`.
pub(crate) trait Runner {
    /// Whether `program` with `args` exited 0 within `timeout`.
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> bool;
}

pub(crate) struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> bool {
        run_with_timeout(program, args, timeout)
    }
}

/// Whether `program` exited 0 within `timeout`. One still running then is killed. Nothing
/// is read from or written to its standard streams.
pub(crate) fn run_with_timeout(program: &str, args: &[String], timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The `[tts]` arguments that make the model write `<prefix>.wav` and play nothing:
/// every `--play` removed, `--file_prefix <prefix> --join_audio --audio_format wav` put
/// before the last `--text` (or at the end when there is none), then `line` (after one
/// space when it starts with `-`, so it is not read as an option). `None` when `[tts]`
/// does not run the model.
pub(crate) fn model_args(tts: &TtsSettings, prefix: &Path, line: &str) -> Option<Vec<String>> {
    let args = tts.args.as_deref().unwrap_or_default();
    let runs_model = tts.command.contains(MODEL_MODULE) || args.iter().any(|a| a == MODEL_MODULE);
    if !runs_model {
        return None;
    }
    let mut model: Vec<String> = args.iter().filter(|a| *a != "--play").cloned().collect();
    let at = model
        .iter()
        .rposition(|a| a == "--text")
        .unwrap_or(model.len());
    let file = [
        "--file_prefix".to_string(),
        prefix.to_string_lossy().into_owned(),
        "--join_audio".to_string(),
        "--audio_format".to_string(),
        "wav".to_string(),
    ];
    model.splice(at..at, file);
    model.push(as_last_argument(line).into_owned());
    Some(model)
}

/// How long a sent voice note is kept. Messages may read the file after `osascript`
/// returns, so a note's folder is removed only once it is this old.
pub(crate) const NOTE_LIFETIME: Duration = Duration::from_secs(600);

/// The one folder Harold keeps its voice notes in, each in a folder of its own.
pub(crate) fn notes_parent() -> PathBuf {
    std::env::temp_dir().join("harold-voice-notes")
}

/// Remove every folder in `parent` last changed `NOTE_LIFETIME` or longer before `now`.
/// A folder whose age cannot be read, or that cannot be removed, is left; a missing
/// `parent` is nothing to do.
pub(crate) fn remove_old_notes(parent: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= NOTE_LIFETIME)
            });
        if old && path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The longest a note's name may be before `.m4a`, in characters.
const NAME_LIMIT: usize = 60;

/// The file name of the voice note of `line`, so the phone shows what it says: its letters
/// and digits, apostrophes removed and every run of anything else one space, cut after the
/// last whole word within `NAME_LIMIT` characters (a longer first word is cut at the limit),
/// then `.m4a`. `voice note.m4a` when the line has no letter or digit. The name never holds
/// `/` or `:`, never starts with `-` or `.` and is at most 244 bytes.
pub(crate) fn note_name(line: &str) -> String {
    let words: String = line
        .chars()
        .filter(|c| !matches!(c, '\'' | '\u{2019}'))
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let mut name = String::new();
    for word in words.split_whitespace() {
        let joined = name.chars().count() + usize::from(!name.is_empty()) + word.chars().count();
        if joined > NAME_LIMIT {
            if name.is_empty() {
                name = word.chars().take(NAME_LIMIT).collect();
            }
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    if name.is_empty() {
        name.push_str("voice note");
    }
    format!("{name}.m4a")
}

/// `<folder>/<note_name(line)>` made from `line`: by the model and `afconvert` when `[tts]` runs
/// the model and the model writes `line.wav`, otherwise by `say`.
fn make_note(
    folder: &Path,
    tts: &TtsSettings,
    line: &str,
    runner: &dyn Runner,
) -> Result<PathBuf, String> {
    let note = folder.join(note_name(line));
    if let Some(args) = model_args(tts, &folder.join("line"), line) {
        let wav = folder.join("line.wav");
        if runner.run(&tts.command, &args, MODEL_TIMEOUT) && wav.is_file() {
            let convert = ["-f", "m4af", "-d", "aac"]
                .map(String::from)
                .into_iter()
                .chain([arg(&wav), arg(&note)])
                .collect::<Vec<_>>();
            if runner.run("afconvert", &convert, MODEL_TIMEOUT) && note.is_file() {
                return Ok(note);
            }
            return Err("afconvert could not make the voice note".into());
        }
        warn!("voice note made with say (the model failed)");
    }
    // The line is read from a file, so a line that starts with `-` is not an option.
    let text = folder.join("line.txt");
    std::fs::write(&text, line)
        .map_err(|error| format!("could not write the voice note text: {error}"))?;
    let say = vec![
        "-f".to_string(),
        arg(&text),
        "-o".to_string(),
        arg(&note),
        "--file-format=m4af".to_string(),
        "--data-format=aac".to_string(),
    ];
    if runner.run("say", &say, MODEL_TIMEOUT) && note.is_file() {
        return Ok(note);
    }
    Err("say could not make the voice note".into())
}

/// Make a voice note of `line` in a new folder under `parent` and hand its path to
/// `send`. Folders in `parent` that are `NOTE_LIFETIME` old at `now` are removed first. A
/// sent note's folder is kept, for Messages to read; it is removed by a later clean-up.
/// `Err` when the note could not be made or `send` failed: that note's folder is removed
/// at once and the caller retries the line.
pub(crate) fn send_voice_note_in(
    parent: &Path,
    now: SystemTime,
    tts: &TtsSettings,
    line: &str,
    runner: &dyn Runner,
    send: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let span = tracing::info_span!(target: crate::telemetry::TRACE_TARGET, "harold.voice_note");
    span.in_scope(|| {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not make the voice notes folder: {error}"))?;
        remove_old_notes(parent, now);
        let folder = parent.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&folder)
            .map_err(|error| format!("could not make the voice note folder: {error}"))?;
        let sent = make_note(&folder, tts, line, runner).and_then(|note| send(&note));
        if sent.is_err() {
            let _ = std::fs::remove_dir_all(&folder);
        }
        sent
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    use super::{
        MODEL_TIMEOUT, NOTE_LIFETIME, Runner, model_args, note_name, notes_parent,
        remove_old_notes, run_with_timeout, send_voice_note_in,
    };
    use crate::outbound::fake::CapturedLogs;
    use crate::settings::TtsSettings;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("harold-voice-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mlx() -> TtsSettings {
        TtsSettings {
            model_revision: crate::settings::KOKORO_REVISION.to_string(),
            command: "/venv/bin/python".into(),
            voice: None,
            args: Some(
                [
                    "-m",
                    "mlx_audio.tts.generate",
                    "--model",
                    "kokoro",
                    "--voice",
                    "af_heart",
                    "--play",
                    "--text",
                ]
                .map(String::from)
                .to_vec(),
            ),
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
        }
    }

    fn say_only() -> TtsSettings {
        TtsSettings {
            model_revision: crate::settings::KOKORO_REVISION.to_string(),
            command: "say".into(),
            args: None,
            ..mlx()
        }
    }

    /// Records each program run and writes the file the real program would, unless the
    /// program is in `failing`.
    struct FakeRunner {
        failing: Vec<&'static str>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn new(failing: &[&'static str]) -> Self {
            Self {
                failing: failing.to_vec(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn programs(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(p, _)| p.clone())
                .collect()
        }

        fn args_of(&self, program: &str) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| p == program)
                .unwrap()
                .1
                .clone()
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|a| a == flag).unwrap();
        &args[at + 1]
    }

    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[String], _timeout: Duration) -> bool {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            if self.failing.contains(&program) {
                return false;
            }
            let written = match program {
                "afconvert" => PathBuf::from(args.last().unwrap()),
                "say" => PathBuf::from(value_after(args, "-o")),
                _ => PathBuf::from(format!("{}.wav", value_after(args, "--file_prefix"))),
            };
            std::fs::write(written, b"audio").unwrap();
            true
        }
    }

    /// Sends through a fake that records the note's name and that it exists.
    fn send_with(
        parent: &Path,
        tts: &TtsSettings,
        line: &str,
        runner: &FakeRunner,
    ) -> (Result<(), String>, Vec<(String, bool)>) {
        let mut sent = Vec::new();
        let result = send_voice_note_in(parent, SystemTime::now(), tts, line, runner, |note| {
            sent.push((
                note.file_name().unwrap().to_string_lossy().into_owned(),
                note.is_file(),
            ));
            Ok(())
        });
        (result, sent)
    }

    /// The folders in `parent`.
    fn folders(parent: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(parent)
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn making_and_sending_a_note_is_one_voice_note_span() {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        crate::telemetry::test_support::install();
        let directory = TestDirectory::new();
        let runner = FakeRunner::new(&[]);
        let around = tracing::info_span!(target: crate::telemetry::TRACE_TARGET, "test.delivery");
        let parent = around.context().span().span_context().span_id();

        let (result, _) =
            around.in_scope(|| send_with(&directory.0, &mlx(), "All green.", &runner));

        assert!(result.is_ok());
        let notes: Vec<_> = crate::telemetry::test_support::spans()
            .into_iter()
            .filter(|span| span.name == "harold.voice_note" && span.parent_span_id == parent)
            .collect();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].attributes.is_empty(), "{:?}", notes[0].attributes);
    }

    #[test]
    fn model_args_drop_play_and_ask_for_one_wav_before_the_text() {
        let args = model_args(&mlx(), Path::new("/tmp/f/line"), "All green.").unwrap();
        assert_eq!(
            args,
            [
                "-m",
                "mlx_audio.tts.generate",
                "--model",
                "kokoro",
                "--voice",
                "af_heart",
                "--file_prefix",
                "/tmp/f/line",
                "--join_audio",
                "--audio_format",
                "wav",
                "--text",
                "All green.",
            ]
        );
    }

    #[test]
    fn model_args_remove_play_wherever_it_is() {
        let mut tts = mlx();
        tts.args = Some(
            ["--play", "-m", "mlx_audio.tts.generate", "--play", "--text"]
                .map(String::from)
                .to_vec(),
        );
        let args = model_args(&tts, Path::new("/f/line"), "hi").unwrap();
        assert!(!args.contains(&"--play".to_string()));
        assert_eq!(args.last().unwrap(), "hi");
        assert_eq!(args[args.len() - 2], "--text");
    }

    #[test]
    fn model_args_pass_a_line_starting_with_a_dash_after_one_space() {
        let args = model_args(&mlx(), Path::new("/f/line"), "- merged pod 3").unwrap();
        assert_eq!(args[args.len() - 2..], ["--text", " - merged pod 3"]);
    }

    #[test]
    fn model_args_are_none_when_tts_does_not_run_the_model() {
        assert_eq!(model_args(&say_only(), Path::new("/f/line"), "hi"), None);
    }

    #[test]
    fn model_note_is_converted_sent_once_and_kept_in_its_own_folder() {
        let parent = TestDirectory::new();
        let runner = FakeRunner::new(&[]);

        let (result, sent) = send_with(&parent.0, &mlx(), "All green.", &runner);

        assert_eq!(result, Ok(()));
        assert_eq!(runner.programs(), ["/venv/bin/python", "afconvert"]);
        let convert = runner.args_of("afconvert");
        assert_eq!(&convert[..4], ["-f", "m4af", "-d", "aac"]);
        assert!(convert[4].ends_with("/line.wav"));
        assert!(convert[5].ends_with("/All green.m4a"));
        assert_eq!(sent, [("All green.m4a".to_string(), true)]);
        // Messages may read the file after osascript returns, so a sent note is kept
        // until a later clean-up finds it older than 10 minutes.
        let kept = folders(&parent.0);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].join("All green.m4a").is_file());
    }

    #[test]
    fn failed_model_falls_back_to_say_and_logs_it() {
        let parent = TestDirectory::new();
        let runner = FakeRunner::new(&["/venv/bin/python"]);
        let logs = CapturedLogs::start();

        let (result, sent) = send_with(&parent.0, &mlx(), "All green.", &runner);

        assert_eq!(result, Ok(()));
        assert_eq!(runner.programs(), ["/venv/bin/python", "say"]);
        assert_eq!(sent, [("All green.m4a".to_string(), true)]);
        assert!(
            logs.text()
                .contains("voice note made with say (the model failed)")
        );
        assert!(!logs.text().contains("All green"));
    }

    #[test]
    fn say_is_used_directly_when_tts_does_not_run_the_model() {
        let parent = TestDirectory::new();
        let runner = FakeRunner::new(&[]);

        let (result, _) = send_with(&parent.0, &say_only(), "All green.", &runner);

        assert_eq!(result, Ok(()));
        assert_eq!(runner.programs(), ["say"]);
    }

    #[test]
    fn say_gets_the_line_from_a_file_not_an_argument() {
        let parent = TestDirectory::new();
        let runner = FakeRunner::new(&[]);
        let line = "- merged pod 3";
        let mut text_file = String::new();

        let result = send_voice_note_in(
            &parent.0,
            SystemTime::now(),
            &say_only(),
            line,
            &runner,
            |_| {
                let args = runner.args_of("say");
                text_file = std::fs::read_to_string(value_after(&args, "-f")).unwrap();
                Ok(())
            },
        );

        assert_eq!(result, Ok(()));
        let args = runner.args_of("say");
        assert!(!args.iter().any(|a| a == line));
        assert!(args.contains(&"--file-format=m4af".to_string()));
        assert!(args.contains(&"--data-format=aac".to_string()));
        assert_eq!(text_file, line);
        assert!(value_after(&args, "-f").ends_with("/line.txt"));
        assert!(value_after(&args, "-o").ends_with("/merged pod 3.m4a"));
    }

    #[test]
    fn note_name_follows_the_naming_rule() {
        // 50 five-letter words and a full stop make a 300-character line.
        let five_letter_words = format!("{}.", ["alpha"; 50].join(" "));
        assert_eq!(five_letter_words.chars().count(), 300);
        let eighty = "a".repeat(80);
        let sixty = "a".repeat(60);
        let cases = [
            (
                "Release install is on task five of six. Nothing needs you.",
                "Release install is on task five of six Nothing needs you.m4a".to_string(),
            ),
            (
                "don't wait / merge: done",
                "dont wait merge done.m4a".into(),
            ),
            ("don’t", "dont.m4a".into()),
            ("-rf ../..", "rf.m4a".into()),
            ("🎉🎉", "voice note.m4a".into()),
            ("'’...", "voice note.m4a".into()),
            ("", "voice note.m4a".into()),
            (
                five_letter_words.as_str(),
                format!("{}.m4a", ["alpha"; 10].join(" ")),
            ),
            (eighty.as_str(), format!("{sixty}.m4a")),
            ("Café à 9h", "Café à 9h.m4a".into()),
            ("Build\n\tdone", "Build done.m4a".into()),
            (".env changed", "env changed.m4a".into()),
        ];
        for (line, name) in cases {
            assert_eq!(note_name(line), name, "{line:?}");
        }
    }

    #[test]
    fn note_name_counts_characters_not_bytes_and_keeps_exactly_60() {
        let exactly = format!("{} {}", "a".repeat(29), "b".repeat(30));
        assert_eq!(note_name(&exactly), format!("{exactly}.m4a"));
        let one_over = format!("{} {}", "a".repeat(29), "b".repeat(31));
        assert_eq!(note_name(&one_over), format!("{}.m4a", "a".repeat(29)));
        let accents = "é".repeat(60);
        assert_eq!(note_name(&accents), format!("{accents}.m4a"));
        assert_eq!(note_name(&"é".repeat(61)), format!("{accents}.m4a"));
    }

    #[test]
    fn a_long_line_name_ends_on_a_whole_word_within_the_limit() {
        let line = "Release install is on task five of six and the next step waits on review";
        let name = note_name(line);
        let stem = name.strip_suffix(".m4a").unwrap();
        assert!(stem.chars().count() <= 60, "{name}");
        assert!(line.starts_with(stem), "{name}");
        assert_eq!(line.as_bytes()[stem.len()], b' ', "{name} ends mid-word");
    }

    #[test]
    fn failing_say_afconvert_or_send_is_an_error_and_leaves_no_folder() {
        for (tts, failing) in [(say_only(), "say"), (mlx(), "afconvert")] {
            let parent = TestDirectory::new();
            let (result, sent) = send_with(&parent.0, &tts, "hi", &FakeRunner::new(&[failing]));
            assert!(result.is_err(), "{failing}");
            assert!(sent.is_empty());
            assert!(folders(&parent.0).is_empty());
        }
        let parent = TestDirectory::new();
        let result = send_voice_note_in(
            &parent.0,
            SystemTime::now(),
            &mlx(),
            "hi",
            &FakeRunner::new(&[]),
            |_| Err("osascript exited unsuccessfully".to_string()),
        );
        assert_eq!(result, Err("osascript exited unsuccessfully".to_string()));
        assert!(folders(&parent.0).is_empty());
    }

    #[test]
    fn clean_up_removes_note_folders_10_minutes_old_or_older_and_keeps_newer_ones() {
        let parent = TestDirectory::new();
        let note = parent.0.join("a-note");
        std::fs::create_dir(&note).unwrap();
        std::fs::write(note.join("line.m4a"), b"audio").unwrap();
        let made = std::fs::metadata(&note).unwrap().modified().unwrap();

        remove_old_notes(&parent.0, made + Duration::from_secs(599));
        assert!(note.is_dir(), "a note under 10 minutes old is kept");

        remove_old_notes(&parent.0, made + NOTE_LIFETIME);
        assert!(!note.exists(), "a note 10 minutes old is removed");

        // A parent that does not exist yet is not an error.
        remove_old_notes(&parent.0.join("missing"), made);
    }

    #[test]
    fn making_a_note_removes_old_note_folders_first() {
        let parent = TestDirectory::new();
        let old = parent.0.join("old-note");
        std::fs::create_dir(&old).unwrap();
        let later = std::fs::metadata(&old).unwrap().modified().unwrap() + NOTE_LIFETIME;

        let result = send_voice_note_in(
            &parent.0,
            later,
            &say_only(),
            "hi",
            &FakeRunner::new(&[]),
            |_| Ok(()),
        );

        assert_eq!(result, Ok(()));
        assert!(!old.exists());
        assert_eq!(folders(&parent.0).len(), 1, "only the new note is left");
    }

    #[test]
    fn notes_are_kept_under_one_harold_folder_in_the_temporary_folder() {
        assert_eq!(
            notes_parent(),
            std::env::temp_dir().join("harold-voice-notes")
        );
        assert_eq!(NOTE_LIFETIME, Duration::from_secs(600));
    }

    #[test]
    fn model_is_given_120_seconds() {
        assert_eq!(MODEL_TIMEOUT, Duration::from_secs(120));
    }

    #[test]
    fn run_with_timeout_kills_a_program_that_runs_too_long() {
        let started = std::time::Instant::now();
        assert!(!run_with_timeout(
            "sleep",
            &["5".to_string()],
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(run_with_timeout("true", &[], Duration::from_secs(5)));
        assert!(!run_with_timeout("false", &[], Duration::from_secs(5)));
        assert!(!run_with_timeout(
            "/no/such/program",
            &[],
            Duration::from_secs(5)
        ));
    }
}
