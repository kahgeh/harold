use std::borrow::Cow;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{info, warn};

use crate::settings::{TtsSettings, get_settings};
use crate::store::TurnCompleted;

fn run_local_model(system_prompt: &str, prompt: &str, max_tokens: u32) -> Option<String> {
    let cfg = get_settings();
    let model = cfg.ai.local_model.as_deref()?;
    let model_dir = cfg.ai.local_model_dir.as_deref()?;

    let out = Command::new("uv")
        .args([
            "run",
            "mlx_lm.generate",
            "--model",
            model,
            "--system-prompt",
            system_prompt,
            "--prompt",
            prompt,
            "--max-tokens",
            &max_tokens.to_string(),
        ])
        .current_dir(model_dir)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let output = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if output.is_empty() {
        return None;
    }

    // Extract content between ========== markers (mlx_lm output format)
    if output.contains("==========") {
        let mut in_content = false;
        let mut lines: Vec<&str> = Vec::new();
        for line in output.lines() {
            if line.trim() == "==========" {
                if in_content {
                    break;
                }
                in_content = true;
            } else if in_content {
                lines.push(line.trim());
            }
        }
        let text = lines
            .join(" ")
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        // Strip <think>...</think> blocks (reasoning models)
        let text = regex_strip_think(&text);
        if text.is_empty() { None } else { Some(text) }
    } else {
        let text = output.trim_matches('"').trim_matches('\'');
        let text = regex_strip_think(text);
        if text.is_empty() {
            None
        } else {
            Some(text.chars().take(200).collect())
        }
    }
}

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

fn build_short_summary(last_user_prompt: &str) -> String {
    let system_prompt = "You are a notification assistant. Given a user's last request, \
         write ONLY a brief 3-8 word summary of what was completed. \
         Do not include any thinking, explanations, or extra text. \
         Output format: Just the summary message.";
    let prompt = format!(
        "User's last request: {}\n\nWrite a 3-8 word summary of what was done:",
        last_user_prompt.chars().take(500).collect::<String>(),
    );
    run_local_model(system_prompt, &prompt, 20).unwrap_or_else(|| "Work complete".into())
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
    #[cfg_attr(not(test), allow(dead_code))] // until the call is wired up (plan Task 9)
    pub(crate) fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// The daemon's stopper, used by `speak_line` and `notify_at_desk`.
#[cfg_attr(not(test), allow(dead_code))] // until the call is wired up (plan Task 9)
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
    let started_at = stopper.generation();
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

/// The turn-end update the desk says, shared by `notify_at_desk` and a call.
#[cfg_attr(not(test), allow(dead_code))] // until the call is wired up (plan Task 9)
pub(crate) fn desk_update_line(last_user_prompt: &str, main_context: &str) -> String {
    let summary = build_short_summary(last_user_prompt);
    format!("{summary} on {main_context} and waiting for further instructions")
}

pub fn notify_at_desk(turn: &TurnCompleted, _trace_id: &str) -> bool {
    let message = desk_update_line(&turn.last_user_prompt, &turn.main_context);
    let tts = &get_settings().tts;
    notify_with_fallback(tts, &message, desk_line_stopper())
}

/// `line` as it is passed as a program's last argument: a line that starts with `-` gets
/// one space in front, so `say` and the model do not read it as an option. The space is
/// not heard.
pub(crate) fn as_last_argument(line: &str) -> Cow<'_, str> {
    if line.starts_with('-') {
        Cow::Owned(format!(" {line}"))
    } else {
        Cow::Borrowed(line)
    }
}

/// Say `line` with the `[tts]` command, falling back as a turn-end update does. Nothing
/// heard is added to the line.
pub(crate) fn speak_line(line: &str) -> bool {
    speak_line_with(&get_settings().tts, line, desk_line_stopper())
}

/// Says `line` with `tts`, returning when it has ended or been stopped; true when it was
/// said or stopped.
pub(crate) fn speak_line_with(tts: &TtsSettings, line: &str, stopper: &LineStopper) -> bool {
    notify_with_fallback(tts, &as_last_argument(line), stopper)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_with_fallback_uses_fallback_when_primary_fails() {
        let tts = TtsSettings {
            command: "/usr/bin/false".into(),
            voice: None,
            args: None,
            fallback_command: Some("/usr/bin/true".into()),
            fallback_voice: None,
            fallback_args: None,
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
            command: "/usr/bin/false".into(),
            voice: None,
            args: None,
            fallback_command: None,
            fallback_voice: None,
            fallback_args: None,
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
        let out = std::env::temp_dir().join(format!("harold-tts-{}", uuid::Uuid::new_v4()));
        let mut tts = writes_last_argument_to(&out);
        let fallback = (tts.command, tts.args);
        tts.command = "/bin/sh".into();
        tts.args = Some(vec!["-c".into(), "sleep 5".into(), "sh".into()]);
        tts.fallback_command = Some(fallback.0);
        tts.fallback_args = fallback.1;
        let stopper = std::sync::Arc::new(LineStopper::default());

        let started = std::time::Instant::now();
        let speaking = {
            let stopper = stopper.clone();
            std::thread::spawn(move || speak_line_with(&tts, "a long line", &stopper))
        };
        std::thread::sleep(Duration::from_millis(200));
        stopper.stop();

        assert!(speaking.join().unwrap());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!out.exists());
    }

    #[test]
    fn desk_update_line_reads_like_the_desk() {
        crate::settings::init_settings_for_test();

        assert_eq!(
            desk_update_line("x", "chief-of-pods"),
            "Work complete on chief-of-pods and waiting for further instructions"
        );
    }
}
