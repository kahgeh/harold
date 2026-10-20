use std::borrow::Cow;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use tracing::{info, warn};

use crate::settings::{TtsSettings, get_settings};
use crate::store::TurnCompleted;
use crate::telemetry::{TRACE_TARGET, instruments};
use crate::voice_input::{Speaker, say_in_open_call};

/// What one run of the local model came to. `Failed` holds the `reason` of the
/// `local model failed` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelRun {
    Answered(String),
    Failed(&'static str),
    NoModel,
}

impl ModelRun {
    fn result(&self) -> &'static str {
        match self {
            ModelRun::Answered(_) => "answered",
            ModelRun::Failed(_) => "failed",
            ModelRun::NoModel => "no_model",
        }
    }
}

/// Runs the local model inside a `harold.local_model` span. No text of the prompt or the
/// answer is logged or recorded.
fn run_local_model(system_prompt: &str, prompt: &str, max_tokens: u32) -> ModelRun {
    let cfg = get_settings();
    let configured = cfg
        .ai
        .local_model
        .as_deref()
        .zip(cfg.ai.local_model_dir.as_deref());
    run_local_model_with(configured, system_prompt, prompt, max_tokens)
}

/// `run_local_model` with the model and its folder given, `None` when none is configured.
fn run_local_model_with(
    configured: Option<(&str, &str)>,
    system_prompt: &str,
    prompt: &str,
    max_tokens: u32,
) -> ModelRun {
    let span = tracing::info_span!(
        target: TRACE_TARGET,
        "harold.local_model",
        result = tracing::field::Empty
    );
    let run = span.in_scope(|| run_model_command(configured, system_prompt, prompt, max_tokens));
    span.record("result", run.result());
    run
}

fn run_model_command(
    configured: Option<(&str, &str)>,
    system_prompt: &str,
    prompt: &str,
    max_tokens: u32,
) -> ModelRun {
    let Some((model, model_dir)) = configured else {
        return ModelRun::NoModel;
    };

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
        .output();
    let out = match out {
        Ok(out) => out,
        Err(error) => return model_failed("spawn_failed", &error.to_string()),
    };
    if !out.status.success() {
        return model_failed("exit_status", &out.status.to_string());
    }
    match model_text(&String::from_utf8_lossy(&out.stdout)) {
        Some(text) => ModelRun::Answered(text),
        None => model_failed("empty_output", ""),
    }
}

/// Logs `local model failed`. `detail` (an exit status or an OS error, never model text)
/// stays in `harold.log`: the field `detail` is not exported.
fn model_failed(reason: &'static str, detail: &str) -> ModelRun {
    warn!(reason = %reason, detail = %detail, "local model failed");
    ModelRun::Failed(reason)
}

/// The model's answer in `stdout`, or `None` when it holds no text.
fn model_text(stdout: &str) -> Option<String> {
    let output = stdout.trim().to_string();
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

/// Who a turn-end update is about: the chief, or the agent whose project is `main_context`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentName {
    Chief,
    Named(String),
}

impl AgentName {
    /// The name without an article, as the sentence must contain it.
    pub(crate) fn bare(&self) -> &str {
        match self {
            AgentName::Chief => "chief",
            AgentName::Named(name) => name,
        }
    }

    /// The name as a sentence says it: `the chief`, `the architect`, `the harold agent`.
    pub(crate) fn spoken(&self) -> String {
        match self {
            AgentName::Chief => "the chief".to_string(),
            AgentName::Named(name) if name == "architect" => "the architect".to_string(),
            AgentName::Named(name) => format!("the {name} agent"),
        }
    }
}

/// Why a turn-end update is the fallback sentence. `as_str` is the `reason` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackReason {
    NoModel,
    ModelFailed,
    TooLong,
    NoName,
}

impl FallbackReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FallbackReason::NoModel => "no_model",
            FallbackReason::ModelFailed => "model_failed",
            FallbackReason::TooLong => "too_long",
            FallbackReason::NoName => "no_name",
        }
    }
}

/// The most words a turn-end update may have.
const MAX_UPDATE_WORDS: usize = 25;

/// `<Name> has finished and is waiting.`, the sentence said when the model gives none.
pub(crate) fn fallback_sentence(agent: &AgentName) -> String {
    format!(
        "{} has finished and is waiting.",
        capitalised(&agent.spoken())
    )
}

fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Makes the model's `answer` one sentence: the text up to the first `.`, `!` or `?` that
/// is followed by a space or ends the text, with one full stop. An empty answer, one over
/// 25 words and one that does not name `agent` are refused. Empty counts as `no_name`.
pub(crate) fn clean_sentence(answer: &str, agent: &AgentName) -> Result<String, FallbackReason> {
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
    if !normal(body).contains(&normal(agent.bare())) {
        return Err(FallbackReason::NoName);
    }
    Ok(format!("{body}."))
}

/// The model's answer for `agent`, from the agent's name, the project and the last request.
fn ask_model(last_user_prompt: &str, agent: &AgentName, project: &str) -> ModelRun {
    let system_prompt = "You are a notification assistant. Write ONE plain sentence, under \
         about 20 words, in the past tense, saying who did what. Name the agent as given. \
         Do not include any thinking, preamble or extra text. \
         Output format: just the sentence.";
    let prompt = format!(
        "Agent: {}\nProject: {}\nLast request: {}\n\nWrite the one sentence:",
        agent.spoken(),
        project,
        last_user_prompt.chars().take(500).collect::<String>(),
    );
    run_local_model(system_prompt, &prompt, 48)
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

/// The daemon's stopper, used by `speak_line` and `notify_at_desk`.
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

/// The turn-end update the desk says, shared by `notify_at_desk` and a call. `channel` is
/// `desk` or `call`.
pub(crate) fn desk_update_line(
    last_user_prompt: &str,
    agent: &AgentName,
    project: &str,
    channel: &'static str,
) -> String {
    update_line_with(agent, channel, || {
        ask_model(last_user_prompt, agent, project)
    })
}

/// `desk_update_line` with the model's run given. The counter and the `turn-end update
/// fell back` line hold the outcome, the reason and the channel only, never the sentence
/// or the model's text.
pub(crate) fn update_line_with(
    agent: &AgentName,
    channel: &'static str,
    run: impl FnOnce() -> ModelRun,
) -> String {
    let refused = match run() {
        ModelRun::Answered(answer) => match clean_sentence(&answer, agent) {
            Ok(sentence) => {
                count_update("model", "none");
                return sentence;
            }
            Err(reason) => reason,
        },
        ModelRun::Failed(_) => FallbackReason::ModelFailed,
        ModelRun::NoModel => FallbackReason::NoModel,
    };
    info!(reason = %refused.as_str(), channel = %channel, "turn-end update fell back");
    count_update("fallback", refused.as_str());
    fallback_sentence(agent)
}

fn count_update(outcome: &'static str, reason: &'static str) {
    instruments().turn_end_updates.add(
        1,
        &[
            KeyValue::new("outcome", outcome),
            KeyValue::new("reason", reason),
        ],
    );
}

/// Says another agent's turn-end update in the open call, or with the `[tts]` command
/// when no call is open. `is_chief` is whether the turn is the chief's pane's.
pub fn notify_at_desk_as(turn: &TurnCompleted, _trace_id: &str, is_chief: bool) -> bool {
    let agent = if is_chief {
        AgentName::Chief
    } else {
        AgentName::Named(turn.main_context.clone())
    };
    let message = desk_update_line(&turn.last_user_prompt, &agent, &turn.main_context, "desk");
    if let Some(said) = say_in_open_call(&message, Speaker::OtherAgent) {
        return said;
    }
    let tts = &get_settings().tts;
    notify_with_fallback(tts, &message, desk_line_stopper())
}

/// `notify_at_desk_as` for a pane that is not the chief's.
pub fn notify_at_desk(turn: &TurnCompleted, trace_id: &str) -> bool {
    notify_at_desk_as(turn, trace_id, false)
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

/// Say `line` in the open call, or with the `[tts]` command, falling back as a turn-end
/// update does, when no call is open. Nothing heard is added to the line.
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
        let architect = AgentName::Named("architect".into());
        let harold = AgentName::Named("harold".into());
        assert_eq!(
            clean_sentence(
                "The architect reviewed and merged Harold's branch.",
                &architect
            ),
            Ok("The architect reviewed and merged Harold's branch.".to_string())
        );
        assert_eq!(
            clean_sentence("\"The architect reviewed it. Then it waited.\"", &architect),
            Ok("The architect reviewed it.".to_string())
        );
        assert_eq!(
            clean_sentence("The harold agent installed v0.11.0", &harold),
            Ok("The harold agent installed v0.11.0.".to_string())
        );
        assert_eq!(
            clean_sentence("<think>hm</think>The architect\nreviewed it", &architect),
            Ok("The architect reviewed it.".to_string())
        );
    }

    #[test]
    fn clean_sentence_refuses_empty_long_and_nameless_answers() {
        let architect = AgentName::Named("architect".into());
        assert_eq!(clean_sentence("", &architect), Err(FallbackReason::NoName));
        let long = format!("The architect {}.", "did a thing ".repeat(12));
        assert_eq!(
            clean_sentence(&long, &architect),
            Err(FallbackReason::TooLong)
        );
        assert_eq!(
            clean_sentence("The agent fixed the wording.", &architect),
            Err(FallbackReason::NoName)
        );
    }

    #[test]
    fn the_name_check_ignores_case_and_hyphens() {
        let architect = AgentName::Named("architect".into());
        let typed = AgentName::Named("questions-typed-panic".into());
        assert!(
            clean_sentence(
                "The Architect reviewed and merged Harold's branch.",
                &architect
            )
            .is_ok()
        );
        assert!(
            clean_sentence("The questions typed panic agent fixed the wording.", &typed).is_ok()
        );
    }

    #[test]
    fn the_fallback_sentence_names_the_agent() {
        assert_eq!(
            fallback_sentence(&AgentName::Chief),
            "The chief has finished and is waiting."
        );
        assert_eq!(
            fallback_sentence(&AgentName::Named("architect".into())),
            "The architect has finished and is waiting."
        );
        assert_eq!(
            fallback_sentence(&AgentName::Named("harold".into())),
            "The harold agent has finished and is waiting."
        );
    }

    /// Serialises the tests that read the process-wide counter.
    static COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn updates_counted(outcome: &str, reason: &str) -> u64 {
        crate::telemetry::test_support::exact_counter_value(
            "harold.turn_end_updates",
            &[("outcome", outcome), ("reason", reason)],
        )
        .unwrap_or(0)
    }

    fn attribute(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    }

    #[test]
    fn a_kept_model_answer_is_one_model_span_and_counts_once() {
        let _serial = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let architect = AgentName::Named("architect".into());
        let before = updates_counted("model", "none");
        let logs = crate::outbound::fake::CapturedLogs::start();

        let line = update_line_with(&architect, "desk", || {
            ModelRun::Answered("MODEL-TEXT-1 The architect reviewed it. SECRET".into())
        });

        println!("evidence: kept: {line}");
        assert_eq!(line, "MODEL-TEXT-1 The architect reviewed it.");
        assert_eq!(updates_counted("model", "none"), before + 1);
        assert!(!logs.text().contains("fell back"));
        assert!(!logs.text().contains("MODEL-TEXT-1"));
        assert!(!crate::telemetry::test_support::exported_text().contains("MODEL-TEXT-1"));
    }

    #[test]
    fn a_refused_answer_falls_back_with_its_reason_and_no_text() {
        let _serial = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let architect = AgentName::Named("architect".into());
        let cases = [
            ("The agent fixed the wording. CANARY-9", "no_name"),
            ("", "no_name"),
            (
                &format!("The architect {}CANARY-9.", "did it ".repeat(30)),
                "too_long",
            ),
        ];
        for (answer, reason) in cases {
            let before = updates_counted("fallback", reason);
            let logs = crate::outbound::fake::CapturedLogs::start();

            let line = update_line_with(&architect, "call", || {
                ModelRun::Answered(answer.to_string())
            });

            println!("evidence: refused ({reason}): {line}");
            assert_eq!(line, "The architect has finished and is waiting.");
            let text = logs.text();
            assert!(
                text.contains(&format!("turn-end update fell back reason={reason}")),
                "{text}"
            );
            assert!(
                !text.contains("CANARY-9") && !text.contains("wording"),
                "{text}"
            );
            assert!(text.contains("channel=call"), "{text}");
            assert_eq!(updates_counted("fallback", reason), before + 1);
        }
        assert!(!crate::telemetry::test_support::exported_text().contains("CANARY-9"));
    }

    #[test]
    fn no_model_and_a_failed_model_fall_back_and_a_model_that_cannot_start_says_why() {
        let _serial = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let chief = AgentName::Chief;
        let no_model_before = updates_counted("fallback", "no_model");
        let failed_before = updates_counted("fallback", "model_failed");
        let logs = crate::outbound::fake::CapturedLogs::start();

        crate::settings::init_settings_for_test();
        let no_model = desk_update_line("fix it", &chief, "chief-of-pods", "desk");
        println!("evidence: no model: {no_model}");
        assert_eq!(no_model, "The chief has finished and is waiting.");

        let run = run_local_model_with(Some(("m", "/nonexistent-harold-test-dir")), "s", "p", 48);
        assert_eq!(run, ModelRun::Failed("spawn_failed"));
        let line = update_line_with(&chief, "desk", || run);
        assert_eq!(line, "The chief has finished and is waiting.");

        let text = logs.text();
        assert!(
            text.contains("local model failed reason=\"spawn_failed\"")
                || text.contains("local model failed reason=spawn_failed"),
            "{text}"
        );
        assert!(
            text.contains("turn-end update fell back reason=model_failed"),
            "{text}"
        );
        assert!(
            text.contains("turn-end update fell back reason=no_model"),
            "{text}"
        );
        assert_eq!(updates_counted("fallback", "no_model"), no_model_before + 1);
        assert_eq!(
            updates_counted("fallback", "model_failed"),
            failed_before + 1
        );
        let model_spans: Vec<_> = crate::telemetry::test_support::spans()
            .into_iter()
            .filter(|s| s.name == "harold.local_model")
            .collect();
        assert!(
            model_spans
                .iter()
                .any(|s| attribute(s, "result").as_deref() == Some("failed"))
        );
        assert!(
            model_spans
                .iter()
                .any(|s| attribute(s, "result").as_deref() == Some("no_model"))
        );
    }

    #[test]
    fn a_model_answer_is_read_from_mlx_lm_output() {
        assert_eq!(
            model_text("loading\n==========\nThe architect left.\n==========\nstats"),
            Some("The architect left.".to_string())
        );
        assert_eq!(model_text("  \n"), None);
    }

    #[test]
    fn with_default_settings_every_agent_gets_the_fallback_sentence() {
        let _serial = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        crate::settings::init_settings_for_test();
        for (agent, project, expected) in [
            (
                AgentName::Chief,
                "chief-of-pods",
                "The chief has finished and is waiting.",
            ),
            (
                AgentName::Named("architect".into()),
                "architect",
                "The architect has finished and is waiting.",
            ),
            (
                AgentName::Named("questions-typed-panic".into()),
                "questions-typed-panic",
                "The questions-typed-panic agent has finished and is waiting.",
            ),
        ] {
            let line = desk_update_line("fix it", &agent, project, "desk");
            assert_eq!(line, expected);
            assert!(!line.contains("waiting for further instructions"));
            assert!(!line.contains(&format!(" on {project}")));
        }
    }

    #[test]
    fn desk_update_line_reads_like_the_desk() {
        let _serial = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        crate::settings::init_settings_for_test();

        assert_eq!(
            desk_update_line("x", &AgentName::Chief, "chief-of-pods", "desk"),
            "The chief has finished and is waiting."
        );
    }
}
