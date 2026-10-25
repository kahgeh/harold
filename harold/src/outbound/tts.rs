use std::borrow::Cow;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tracing::{info, warn};

use crate::settings::{TtsEngine, TtsSettings, get_settings};
use crate::tts::play::{PLAYER, TempWav};
use crate::tts::{Audio, Caller, TtsServer};
use crate::voice_input::{Speaker, say_in_open_call};

pub(crate) fn regex_strip_think(text: &str) -> String {
    // Remove <think>...</think> blocks emitted by reasoning models like Qwen3.
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        if let Some(end) = rest.find("</think>") {
            rest = rest[end + "</think>".len()..].trim_start();
        } else {
            rest = "";
            break;
        }
    }
    result.push_str(rest);
    result.trim().to_string()
}

/// Why the model's answer is refused. `as_str` is the `reason` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackReason {
    TooLong,
    NoName,
}

impl FallbackReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FallbackReason::TooLong => "too_long",
            FallbackReason::NoName => "no_name",
        }
    }
}

/// The most words a turn-end update may have.
const MAX_UPDATE_WORDS: usize = 25;

/// The sentence spoken when the model gives none.
pub(crate) fn fallback_sentence() -> &'static str {
    "The chief has finished and is waiting."
}

/// Makes the model's `answer` one sentence: the text up to the first `.`, `!` or `?` that
/// is followed by a space or ends the text, with one full stop. An empty answer, one over
/// 25 words and one that does not name the chief are refused. Empty counts as `no_name`.
pub(crate) fn clean_sentence(answer: &str) -> Result<String, FallbackReason> {
    let joined = regex_strip_think(answer)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let joined = joined.trim_matches(|c| c == '"' || c == '\'').trim();
    let chars: Vec<(usize, char)> = joined.char_indices().collect();
    let end = chars
        .iter()
        .enumerate()
        .find(|(i, (_, c))| {
            matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|(_, next)| *next == ' ')
        })
        .map_or(joined.len(), |(_, (at, _))| *at);
    let body = joined[..end].trim();
    if body.is_empty() {
        return Err(FallbackReason::NoName);
    }
    if body.split_whitespace().count() > MAX_UPDATE_WORDS {
        return Err(FallbackReason::TooLong);
    }
    let normal = |text: &str| text.to_lowercase().replace('-', " ");
    if !normal(body).contains("chief") {
        return Err(FallbackReason::NoName);
    }
    Ok(format!("{body}."))
}

/// What the model is given for a turn-end update: its instructions and its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateRequest {
    pub system_prompt: String,
    pub prompt: String,
}

/// The instructions and the input for the chief's turn-end update, from the agent's
/// name, the project and the first 500 characters of the last request.
pub(crate) fn update_request(last_user_prompt: &str, project: &str) -> UpdateRequest {
    let system_prompt = "You are a notification assistant. Write ONE plain sentence, under \
         about 20 words, in the past tense, saying who did what. Name the agent as given. \
         Do not include any thinking, preamble or extra text. \
         Output format: just the sentence."
        .to_string();
    let prompt = format!(
        "Agent: the chief\nProject: {}\nLast request: {}\n\nWrite the one sentence:",
        project,
        last_user_prompt.chars().take(500).collect::<String>(),
    );
    UpdateRequest {
        system_prompt,
        prompt,
    }
}

/// How long a line being said waits between checks that it has ended or been stopped.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Stops the line a `[tts]` command is saying. A stopped line counts as said: it is not
/// passed to the fallback and not retried.
#[derive(Default)]
pub(crate) struct LineStopper {
    generation: AtomicU64,
}

impl LineStopper {
    pub(crate) fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// The daemon's stopper, used by `speak_line` and the call's spoken lines.
pub(crate) fn desk_line_stopper() -> &'static LineStopper {
    static STOPPER: OnceLock<LineStopper> = OnceLock::new();
    STOPPER.get_or_init(LineStopper::default)
}

/// What a finished or stopped `[tts]` command came to.
enum Said {
    Ended(bool),
    Stopped,
}

fn run_tts_command(
    command: &str,
    args: Option<&Vec<String>>,
    voice: Option<&String>,
    message: &str,
    stopper: &LineStopper,
    started_at: u64,
) -> std::io::Result<Said> {
    if stopper.generation() != started_at {
        return Ok(Said::Stopped);
    }
    let mut cmd = Command::new(command);
    if let Some(extra_args) = args {
        cmd.args(extra_args);
    }
    if let Some(voice) = voice {
        cmd.args(["-v", voice]);
    }

    let mut child = cmd.arg(message).spawn()?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Said::Ended(status.success()));
        }
        if stopper.generation() != started_at {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Said::Stopped);
        }
        std::thread::sleep(STOP_POLL);
    }
}

fn notify_with_fallback(tts: &TtsSettings, message: &str, stopper: &LineStopper) -> bool {
    notify_with_fallback_from(tts, message, stopper, stopper.generation())
}

/// `notify_with_fallback` for a line that started at generation `started_at`: a stop since
/// then ends it before the next command is spawned.
fn notify_with_fallback_from(
    tts: &TtsSettings,
    message: &str,
    stopper: &LineStopper,
    started_at: u64,
) -> bool {
    match run_tts_command(
        &tts.command,
        tts.args.as_ref(),
        tts.voice.as_ref(),
        message,
        stopper,
        started_at,
    ) {
        Ok(Said::Ended(true)) => {
            info!("TTS notification sent");
            return true;
        }
        Ok(Said::Stopped) => {
            info!("TTS line stopped");
            return true;
        }
        Ok(Said::Ended(false)) => warn!(command = %tts.command, "TTS command failed"),
        Err(e) => warn!(error = %e, command = %tts.command, "TTS command failed to start"),
    }
    run_fallback(tts, message, stopper, started_at)
}

/// Says `message` with the `[tts]` fallback command; false when none is set or it fails.
fn run_fallback(tts: &TtsSettings, message: &str, stopper: &LineStopper, started_at: u64) -> bool {
    let Some(fallback_command) = &tts.fallback_command else {
        return false;
    };

    match run_tts_command(
        fallback_command,
        tts.fallback_args.as_ref(),
        tts.fallback_voice.as_ref(),
        message,
        stopper,
        started_at,
    ) {
        Ok(Said::Ended(true)) => {
            info!("fallback TTS notification sent");
            true
        }
        Ok(Said::Stopped) => {
            info!("fallback TTS line stopped");
            true
        }
        Ok(Said::Ended(false)) => {
            warn!(command = %fallback_command, "fallback TTS command failed");
            false
        }
        Err(e) => {
            warn!(
                error = %e,
                command = %fallback_command,
                "fallback TTS command failed to start"
            );
            false
        }
    }
}

/// Says `line` through the TTS server: makes the audio, writes it to a temporary WAV file
/// (mode 0600), plays it with `player` and deletes the file. A failure of the server or of
/// the player runs the `[tts]` fallback command. A stop since the line started ends it, and
/// counts as said.
fn speak_through_server(
    tts: &TtsSettings,
    line: &str,
    stopper: &LineStopper,
    server: Option<&Arc<TtsServer>>,
    player: &str,
) -> bool {
    let started_at = stopper.generation();
    let message = as_last_argument(line);
    let Some(server) = server else {
        warn!(reason = "worker_failed", caller = "desk", "tts failed");
        return run_fallback(tts, &message, stopper, started_at);
    };
    // The server logs why a line failed.
    let Ok(audio) = server.speak_all_blocking(Caller::Desk, line) else {
        return run_fallback(tts, &message, stopper, started_at);
    };
    if stopper.generation() != started_at {
        info!("TTS line stopped");
        return true;
    }
    match play_audio(&audio, player, stopper, started_at) {
        Ok(Said::Ended(true)) => {
            info!("TTS notification sent");
            true
        }
        Ok(Said::Stopped) => {
            info!("TTS line stopped");
            true
        }
        Ok(Said::Ended(false)) => {
            warn!(command = player, "TTS player failed");
            run_fallback(tts, &message, stopper, started_at)
        }
        Err(error) => {
            warn!(error = %error, command = player, "TTS player failed to start");
            run_fallback(tts, &message, stopper, started_at)
        }
    }
}

/// Plays `audio` with `player` from a temporary WAV file, which is deleted after.
fn play_audio(
    audio: &Audio,
    player: &str,
    stopper: &LineStopper,
    started_at: u64,
) -> std::io::Result<Said> {
    let wav = TempWav::write(&std::env::temp_dir(), audio)?;
    let path = wav.path().display().to_string();
    run_tts_command(player, None, None, &path, stopper, started_at)
}

/// `line` as it is passed as a program's last argument: a line that starts with `-` gets
/// one space in front, so `say` does not read it as an option. The space is
/// not heard.
pub(crate) fn as_last_argument(line: &str) -> Cow<'_, str> {
    if line.starts_with('-') {
        Cow::Owned(format!(" {line}"))
    } else {
        Cow::Borrowed(line)
    }
}

/// Say `line` in the open call, or at the desk with the `[tts]` engine (the command, or the TTS
/// server), falling back to the fallback command, when no call is open. Nothing heard is added to the line.
pub(crate) fn speak_line(line: &str) -> bool {
    speak_line_through(&get_settings().tts, line)
}

/// `speak_line` with `tts` as the `[tts]` command.
fn speak_line_through(tts: &TtsSettings, line: &str) -> bool {
    if let Some(said) = say_in_open_call(line, Speaker::Chief) {
        return said;
    }
    speak_line_with(tts, line, desk_line_stopper())
}

/// Says `line` with `tts`, returning when it has ended or been stopped; true when it was
/// said or stopped. With `engine = "server"` the line goes through the TTS server.
pub(crate) fn speak_line_with(tts: &TtsSettings, line: &str, stopper: &LineStopper) -> bool {
    match tts.engine {
        TtsEngine::Command => notify_with_fallback(tts, &as_last_argument(line), stopper),
        TtsEngine::Server => {
            speak_through_server(tts, line, stopper, crate::tts::server().as_ref(), PLAYER)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_with_fallback_uses_fallback_when_primary_fails() {
        let tts = TtsSettings {
            model_revision: crate::settings::KOKORO_REVISION.to_string(),
            command: "/usr/bin/false".into(),
            voice: None,
            args: None,
            fallback_command: Some("/usr/bin/true".into()),
            fallback_voice: None,
            fallback_args: None,
            ..TtsSettings::default()
        };

        assert!(notify_with_fallback(
            &tts,
            "test message",
            &LineStopper::default()
        ));
    }

    #[test]
    fn notify_with_fallback_fails_without_fallback() {
        let tts = TtsSettings {
            model_revision: crate::settings::KOKORO_REVISION.to_string(),
            command: "/usr/bin/false".into(),
            voice: None,
            args: None,
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
            ..TtsSettings::default()
        };

        assert!(!notify_with_fallback(
            &tts,
            "test message",
            &LineStopper::default()
        ));
    }

    /// A `[tts]` that writes its last argument to `out`.
    fn writes_last_argument_to(out: &std::path::Path) -> TtsSettings {
        TtsSettings {
            model_revision: crate::settings::KOKORO_REVISION.to_string(),
            command: "/bin/sh".into(),
            voice: None,
            args: Some(vec![
                "-c".into(),
                format!("printf %s \"$1\" > '{}'", out.display()),
                "sh".into(),
            ]),
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
            ..TtsSettings::default()
        }
    }

    #[test]
    fn a_line_starting_with_a_dash_gets_one_space_in_front() {
        assert_eq!(as_last_argument("-3 tests failing"), " -3 tests failing");
        assert_eq!(as_last_argument("a-b"), "a-b");
        assert_eq!(as_last_argument(""), "");
    }

    #[test]
    fn spoken_line_starting_with_a_dash_is_not_passed_as_an_option() {
        let out = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let tts = writes_last_argument_to(&out);

        assert!(speak_line_with(&tts, "-o x", &LineStopper::default()));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), " -o x");
        assert!(speak_line_with(&tts, "All green.", &LineStopper::default()));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "All green.");
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn speak_line_goes_to_tts_when_no_call_is_open() {
        let out = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let tts = writes_last_argument_to(&out);

        assert_eq!(say_in_open_call("All green.", Speaker::Chief), None);
        assert!(speak_line_through(&tts, "All green."));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "All green.");
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn tts_command_receives_exactly_the_line_as_its_last_argument() {
        let out = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let tts = writes_last_argument_to(&out);

        assert!(notify_with_fallback(
            &tts,
            "Release install is done. Nothing needs you.",
            &LineStopper::default()
        ));
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "Release install is done. Nothing needs you."
        );
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn a_stopped_line_counts_as_said_and_skips_the_fallback() {
        let dir = std::env::temp_dir();
        let out = dir.join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let marker = dir.join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let mut tts = writes_last_argument_to(&out);
        tts.fallback_command = Some(tts.command.clone());
        tts.fallback_args = tts.args.take();
        tts.command = "/bin/sh".into();
        tts.args = Some(vec![
            "-c".into(),
            "touch \"$1\"; sleep 5".into(),
            "sh".into(),
            marker.display().to_string(),
        ]);
        let stopper = std::sync::Arc::new(LineStopper::default());

        let started = std::time::Instant::now();
        let speaking = {
            let stopper = stopper.clone();
            std::thread::spawn(move || speak_line_with(&tts, "a long line", &stopper))
        };
        while !marker.exists() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "the primary command never started");
        stopper.stop();

        assert!(speaking.join().unwrap());
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!out.exists());
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn a_stop_before_a_command_is_spawned_spawns_nothing() {
        let primary_out = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let fallback_out =
            std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let mut tts = writes_last_argument_to(&primary_out);
        let fallback = writes_last_argument_to(&fallback_out);
        tts.fallback_command = Some(fallback.command);
        tts.fallback_args = fallback.args;
        let stopper = LineStopper::default();
        let started_at = stopper.generation();
        stopper.stop();

        assert!(notify_with_fallback_from(
            &tts, "line", &stopper, started_at
        ));
        assert!(!primary_out.exists());
        assert!(!fallback_out.exists());
    }

    #[test]
    fn clean_sentence_keeps_one_whole_sentence() {
        assert_eq!(
            clean_sentence("The chief reviewed and merged Harold's branch."),
            Ok("The chief reviewed and merged Harold's branch.".to_string())
        );
        assert_eq!(
            clean_sentence("\"The chief reviewed it. Then it waited.\""),
            Ok("The chief reviewed it.".to_string())
        );
        assert_eq!(
            clean_sentence("The chief installed v0.11.0"),
            Ok("The chief installed v0.11.0.".to_string())
        );
        assert_eq!(
            clean_sentence("<think>hm</think>The chief\nreviewed it"),
            Ok("The chief reviewed it.".to_string())
        );
    }

    #[test]
    fn clean_sentence_refuses_empty_long_and_nameless_answers() {
        assert_eq!(clean_sentence(""), Err(FallbackReason::NoName));
        let long = format!("The chief {}.", "did a thing ".repeat(12));
        assert_eq!(clean_sentence(&long), Err(FallbackReason::TooLong));
        assert_eq!(
            clean_sentence("The agent fixed the wording."),
            Err(FallbackReason::NoName)
        );
    }

    #[test]
    fn the_name_check_ignores_case() {
        assert!(clean_sentence("The Chief reviewed and merged Harold's branch.").is_ok());
    }

    #[test]
    fn the_fallback_sentence_names_the_chief() {
        assert_eq!(
            fallback_sentence(),
            "The chief has finished and is waiting."
        );
        assert!(clean_sentence(fallback_sentence()).is_ok());
    }

    #[test]
    fn the_update_request_holds_the_name_the_project_and_500_characters_of_the_request() {
        let long = "x".repeat(600);
        let request = update_request(&long, "chief-of-pods");
        assert!(request.system_prompt.contains("Name the agent as given"));
        assert_eq!(
            request.prompt,
            format!(
                "Agent: the chief\nProject: chief-of-pods\nLast request: {}\n\nWrite the one sentence:",
                "x".repeat(500)
            )
        );
    }

    use crate::tts::play::wav_bytes;
    use crate::tts::test_support::fixture;
    use std::os::unix::fs::PermissionsExt;

    /// A player that copies the WAV file it is given to `<out>.wav` and notes its path.
    fn copying_player(folder: &std::path::Path) -> String {
        let player = folder.join("player");
        std::fs::write(
            &player,
            "#!/bin/sh\ncp \"$1\" \"$0.wav\"\nprintf %s \"$1\" > \"$0.path\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();
        player.display().to_string()
    }

    fn server_tts(out: &std::path::Path) -> TtsSettings {
        TtsSettings {
            engine: TtsEngine::Server,
            ..writes_last_argument_to_as_fallback(out)
        }
    }

    /// The `[tts]` whose fallback writes its last argument to `out`.
    fn writes_last_argument_to_as_fallback(out: &std::path::Path) -> TtsSettings {
        let fallback = writes_last_argument_to(out);
        TtsSettings {
            command: "/usr/bin/false".into(),
            fallback_command: Some(fallback.command),
            fallback_args: fallback.args,
            ..TtsSettings::default()
        }
    }

    fn scratch_folder() -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("harold-desk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_the_server_engine_a_line_goes_through_the_server_and_plays_from_a_temporary_wav()
    {
        let fixture = fixture().await;
        let folder = scratch_folder();
        let player = copying_player(&folder);
        let tts = server_tts(&folder.join("fallback-out"));
        let server = fixture.server.clone();

        let said = tokio::task::spawn_blocking(move || {
            speak_through_server(
                &tts,
                "All green.",
                &LineStopper::default(),
                Some(&server),
                &player,
            )
        })
        .await
        .unwrap();

        assert!(said);
        let played = std::fs::read(folder.join("player.wav")).unwrap();
        let expected = wav_bytes(&Audio {
            sample_rate: 24_000,
            pcm: vec![0, 0, 0, 0, 1, 0, 1, 0, 2, 0, 2, 0],
        });
        assert_eq!(played, expected);
        // The fallback did not run, and the temporary file is gone after the line plays.
        assert!(!folder.join("fallback-out").exists());
        let path = std::fs::read_to_string(folder.join("player.path")).unwrap();
        assert!(!std::path::Path::new(&path).exists());
        assert!(path.contains("harold-tts-") && path.ends_with(".wav"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_failure_runs_the_fallback_command_with_the_line() {
        let fixture = fixture().await;
        fixture
            .script
            .lines
            .lock()
            .unwrap()
            .push(crate::tts::test_support::Next::Exit);
        let folder = scratch_folder();
        let out = folder.join("fallback-out");
        let tts = server_tts(&out);
        let player = copying_player(&folder);
        let server = fixture.server.clone();

        let said = tokio::task::spawn_blocking(move || {
            speak_through_server(
                &tts,
                "-3 tests failing",
                &LineStopper::default(),
                Some(&server),
                &player,
            )
        })
        .await
        .unwrap();

        assert!(said);
        // The fallback gets the line as `command` does.
        assert_eq!(std::fs::read_to_string(&out).unwrap(), " -3 tests failing");
        assert!(!folder.join("player.wav").exists());
    }

    #[test]
    fn without_a_started_server_the_fallback_speaks_and_with_none_the_line_fails() {
        let folder = scratch_folder();
        let out = folder.join("fallback-out");
        let tts = server_tts(&out);

        assert!(speak_through_server(
            &tts,
            "Hello.",
            &LineStopper::default(),
            None,
            "/usr/bin/true"
        ));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "Hello.");

        let mut without = server_tts(&folder.join("none"));
        without.fallback_command = None;
        assert!(!speak_through_server(
            &without,
            "Hello.",
            &LineStopper::default(),
            None,
            "/usr/bin/true"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_player_that_fails_runs_the_fallback() {
        let fixture = fixture().await;
        let folder = scratch_folder();
        let out = folder.join("fallback-out");
        let tts = server_tts(&out);
        let server = fixture.server.clone();

        let said = tokio::task::spawn_blocking(move || {
            speak_through_server(
                &tts,
                "Hello.",
                &LineStopper::default(),
                Some(&server),
                "/usr/bin/false",
            )
        })
        .await
        .unwrap();

        assert!(said);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "Hello.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_line_stopped_while_it_plays_counts_as_said_and_skips_the_fallback() {
        let fixture = fixture().await;
        let folder = scratch_folder();
        let marker = folder.join("started");
        let player = folder.join("slow-player");
        std::fs::write(
            &player,
            format!("#!/bin/sh\ntouch '{}'\nsleep 5\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = folder.join("fallback-out");
        let tts = server_tts(&out);
        let stopper = Arc::new(LineStopper::default());
        let server = fixture.server.clone();
        let speaking = {
            let stopper = stopper.clone();
            let player = player.display().to_string();
            tokio::task::spawn_blocking(move || {
                speak_through_server(&tts, "A long line.", &stopper, Some(&server), &player)
            })
        };
        let started = std::time::Instant::now();
        while !marker.exists() && started.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "the player never started");
        stopper.stop();

        assert!(speaking.await.unwrap());
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!out.exists());
    }

    #[test]
    fn the_server_engine_without_a_started_server_runs_the_fallback_through_speak_line_with() {
        let folder = scratch_folder();
        let out = folder.join("fallback-out");
        let tts = server_tts(&out);

        assert!(speak_line_with(&tts, "Hi.", &LineStopper::default()));

        assert_eq!(std::fs::read_to_string(&out).unwrap(), "Hi.");
    }

    #[test]
    fn the_command_engine_never_asks_the_server() {
        let out = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let mut tts = writes_last_argument_to(&out);
        tts.engine = TtsEngine::Command;

        assert!(speak_line_with(
            &tts,
            "Said by the command.",
            &LineStopper::default()
        ));

        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "Said by the command."
        );
        let _ = std::fs::remove_file(out);
    }
}
