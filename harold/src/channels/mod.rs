// Without `imessage`, only the web app uses the shared text helpers.
#![cfg_attr(not(feature = "imessage"), allow(dead_code))]

#[cfg(feature = "imessage")]
pub(crate) mod imessage;

use std::process::Command;
#[cfg(feature = "imessage")]
use std::sync::Arc;

#[cfg(feature = "imessage")]
use tokio::sync::watch;
use tracing::{info, warn};

use crate::inbound::AgentAddress;
#[cfg(feature = "imessage")]
use crate::inbound::ownership::agent_message_text;
use crate::settings::get_settings;
#[cfg(feature = "imessage")]
use crate::store::HaroldStore;
use crate::store::{AwayMessageRequested, TurnCompleted};
use crate::util::ai_cli_env;

pub(crate) enum AwayNotification {
    Sent(AgentAddress),
    Skipped,
}

// ---------------------------------------------------------------------------
// Shared utilities — used by the iMessage channel
// ---------------------------------------------------------------------------

/// Split the trailing question off the assistant message body. The question is the last
/// sentence, and only when the body ends with it and it ends in `?`. Any other body, such as
/// one with text after its last question, comes back whole with no question.
pub(crate) fn split_body(body: &str) -> (&str, Option<&str>) {
    let body = body.trim();
    if !body.ends_with('?') {
        return (body, None);
    }
    // Byte indices are safe: the sentence end is a single-byte ASCII char followed by a space.
    let before_question = &body[..body.len() - 1];
    let Some(end) = before_question
        .char_indices()
        .filter(|&(i, c)| matches!(c, '.' | '!' | '?') && before_question[i + 1..].starts_with(' '))
        .map(|(i, _)| i)
        .next_back()
    else {
        return (body, None);
    };
    (body[..=end].trim(), Some(body[end + 1..].trim()))
}

/// The most characters a turn-end update takes on iMessage.
pub(crate) const TURN_END_TEXT_LIMIT: usize = 280;

/// Whether `chars[i]` ends a sentence: `.`, `!` or `?` followed by a space or the end.
fn is_sentence_end(chars: &[char], i: usize) -> bool {
    matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| *c == ' ')
}

/// Fit `text` into `limit` characters. Text that fits is returned as it is. Longer text is
/// cut after the last sentence that fits, with " …" after it; else after the last whole
/// word, with "…"; a single word longer than the limit is cut inside the word.
pub(crate) fn fit_turn_end_text(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let sentence_room = limit.saturating_sub(2);
    if let Some(end) = (0..sentence_room)
        .rev()
        .find(|&i| is_sentence_end(&chars, i))
    {
        return format!("{} …", chars[..=end].iter().collect::<String>());
    }
    let word_room = limit.saturating_sub(1);
    let word_end = if chars[word_room].is_whitespace() {
        word_room
    } else {
        chars[..word_room]
            .iter()
            .rposition(|c| c.is_whitespace())
            .unwrap_or(word_room)
    };
    let kept: String = chars[..word_end].iter().collect();
    format!("{}…", kept.trim_end())
}

/// Fit `text` for iMessage, and report a cut: `true` with an info line that
/// holds the channel, the pane and the length before the cut, never the text.
pub(crate) fn fit_and_report(text: &str, channel: &str, pane_id: &str) -> (String, bool) {
    let count = text.chars().count();
    if count <= TURN_END_TEXT_LIMIT {
        return (text.to_string(), false);
    }
    info!(channel, pane_id, count, "turn-end update cut");
    (fit_turn_end_text(text, TURN_END_TEXT_LIMIT), true)
}

/// Record on the current `harold.delivery` span whether the update was cut.
pub(crate) fn record_text_cut(cut: bool) {
    tracing::Span::current().record("text_cut", cut);
}

/// Split the trailing question off the whole `body`, then fit the main text and the question
/// each to the limit. Returns the main text, the question and whether either was cut.
pub(crate) fn split_and_fit(
    body: &str,
    channel: &str,
    pane_id: &str,
) -> (String, Option<String>, bool) {
    let (main, question) = split_body(body);
    let (main, main_cut) = fit_and_report(main, channel, pane_id);
    let Some(question) = question else {
        return (main, None, main_cut);
    };
    let (question, question_cut) = fit_and_report(question, channel, pane_id);
    (main, Some(question), main_cut || question_cut)
}

/// Summarise `assistant_message` for notification delivery using the AI CLI. The text is
/// not cut: falls back to the whole reply if the CLI is not configured or fails.
pub(crate) fn summarise_for_notification(
    assistant_message: &str,
    last_user_prompt: &str,
) -> String {
    summarise_with(
        get_settings().ai.cli_path.as_deref(),
        assistant_message,
        last_user_prompt,
    )
}

/// The fields of the AI CLI's `--output-format json` result that summarising reads.
#[derive(serde::Deserialize)]
struct CliReply {
    subtype: Option<String>,
    is_error: Option<bool>,
    result: Option<String>,
}

const SUBTYPE_MAX_CHARS: usize = 40;

/// A subtype is logged only when it is a short name of `a-z`, `0-9` and `_`, so the CLI
/// cannot put message text on the line.
fn clean_subtype(subtype: &str) -> &str {
    let fits = !subtype.is_empty()
        && subtype.len() <= SUBTYPE_MAX_CHARS
        && subtype
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if fits { subtype } else { "other" }
}

/// A question sentence that the check removed from a summary.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DroppedQuestion {
    /// Length of the removed sentence, in characters.
    pub(crate) chars: usize,
    /// Best share (0 to 100) of its content words found in one reply question.
    pub(crate) matched_share: u32,
}

/// Split text into sentences. A sentence ends at `.`, `?` or `!` before a space or the end.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let ends = matches!(c, '.' | '?' | '!')
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        if ends {
            let end = i + c.len_utf8();
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            start = end;
        }
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        sentences.push(rest);
    }
    sentences
}

/// Content words: 4 or more letters, lower case, with punctuation removed.
fn content_words(sentence: &str) -> std::collections::HashSet<String> {
    sentence
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect()
}

/// Remove each question sentence of the summary that matches no question of the reply.
///
/// A summary question matches when at least 60 percent of its content words are in
/// one question of the reply. The text returned is empty when nothing is left.
pub(crate) fn drop_unmatched_questions(
    summary: &str,
    reply: &str,
) -> (String, Vec<DroppedQuestion>) {
    let reply_questions: Vec<_> = split_sentences(reply)
        .into_iter()
        .filter(|s| s.ends_with('?'))
        .map(content_words)
        .collect();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for sentence in split_sentences(summary) {
        if !sentence.ends_with('?') {
            kept.push(sentence);
            continue;
        }
        let words = content_words(sentence);
        let share = reply_questions
            .iter()
            .map(|q| {
                if words.is_empty() {
                    0
                } else {
                    (words.iter().filter(|w| q.contains(*w)).count() * 100 / words.len()) as u32
                }
            })
            .max()
            .unwrap_or(0);
        if share >= 60 {
            kept.push(sentence);
        } else {
            dropped.push(DroppedQuestion {
                chars: sentence.chars().count(),
                matched_share: share,
            });
        }
    }
    (kept.join(" "), dropped)
}

/// Apply the question check to the model summary. Log each removed sentence.
/// When nothing is left, use the whole reply with newlines turned to spaces.
pub(crate) fn finish_summary(summary: &str, reply: &str) -> String {
    let (text, dropped) = drop_unmatched_questions(summary, reply);
    let fell_back = text.is_empty();
    for question in &dropped {
        info!(
            reason = "question_not_in_reply",
            chars = question.chars,
            matched_share = question.matched_share,
            fell_back,
            "summary question dropped"
        );
    }
    if fell_back {
        reply.replace('\n', " ")
    } else {
        text
    }
}

/// The prompt that asks the model for the phone summary.
fn build_summary_prompt(safe_prompt: &str, safe_msg: &str) -> String {
    format!(
        "You are writing a phone notification summary.\n\n\
         USER ASKED:\n<prompt>\n{safe_prompt}\n</prompt>\n\n\
         ASSISTANT REPLIED:\n<message>\n{safe_msg}\n</message>\n\n\
         Write 2-3 plain sentences summarising what was done and the outcome.\n\
         Preserve any question the assistant asked.\n\
         Add no question or request that the assistant's reply does not contain.\n\
         Use the user's words only to understand the reply. Never answer them or ask them anything.\n\
         No code, no markdown, no jargon. Keep it under 280 characters."
    )
}

/// [`summarise_for_notification`] with the AI CLI path given.
pub(crate) fn summarise_with(
    cli: Option<&str>,
    assistant_message: &str,
    last_user_prompt: &str,
) -> String {
    let whole_reply = || assistant_message.replace('\n', " ");
    let Some(cli) = cli else {
        return whole_reply();
    };

    let safe_msg = assistant_message
        .replace("</message>", "")
        .replace("</prompt>", "");
    let safe_prompt = last_user_prompt
        .replace("</prompt>", "")
        .replace("</message>", "");
    let prompt = build_summary_prompt(&safe_prompt, &safe_msg);

    let out = Command::new(cli)
        .args([
            "-p",
            &prompt,
            "--model",
            "sonnet",
            "--max-turns",
            "1",
            "--output-format",
            "json",
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .env_remove("CLAUDECODE")
        .envs(ai_cli_env())
        .output();

    match out {
        Ok(o) => {
            let reply = serde_json::from_slice::<CliReply>(&o.stdout).ok();
            if o.status.success()
                && let Some(reply) = &reply
                && reply.is_error == Some(false)
                && reply.subtype.as_deref() == Some("success")
            {
                let summary = reply.result.as_deref().unwrap_or("").trim();
                return if summary.is_empty() {
                    whole_reply()
                } else {
                    finish_summary(&summary.replace('\n', " "), assistant_message)
                };
            }
            let stderr = String::from_utf8_lossy(&o.stderr);
            let subtype = match &reply {
                Some(r) => r.subtype.as_deref().map_or("other", clean_subtype),
                None => "unreadable_json",
            };
            warn!(
                status = %o.status,
                exit_code = o.status.code(),
                subtype,
                is_error = reply.as_ref().and_then(|r| r.is_error),
                stderr = %stderr.chars().take(200).collect::<String>(),
                "summarise_for_notification: AI CLI failed, falling back to the whole reply"
            );
            whole_reply()
        }
        Err(e) => {
            warn!(error = %e, "summarise_for_notification: failed to spawn AI CLI, falling back to the whole reply");
            whole_reply()
        }
    }
}

// ---------------------------------------------------------------------------
// Sending and listening — iMessage is the one away channel
// ---------------------------------------------------------------------------

/// Start the inbound message listener.
#[cfg(feature = "imessage")]
pub async fn listen_for_inbound_messages(store: Arc<HaroldStore>, shutdown: watch::Receiver<()>) {
    imessage::listen(store, shutdown).await;
}

/// What the iMessage wrappers answer in a build without `imessage`: that channel is never set up.
#[cfg(not(feature = "imessage"))]
const NO_IMESSAGE: &str = "this Harold is built without iMessage";

/// Send a plain message to the away device.
#[cfg(feature = "imessage")]
pub fn send(msg: &str) -> Result<(), String> {
    send_unless_paused(
        msg,
        crate::outbound::is_messaging_paused(),
        imessage::send_imessage,
    )
}

/// Send a plain message to the away device: a build without `imessage` has none.
#[cfg(not(feature = "imessage"))]
pub fn send(_msg: &str) -> Result<(), String> {
    Err(NO_IMESSAGE.into())
}

/// Hand `msg` to `deliver` unless messaging is paused. A paused send reports success:
/// the reply is dropped on purpose, so the caller must not retry it.
#[cfg(feature = "imessage")]
fn send_unless_paused(
    msg: &str,
    messaging_paused: bool,
    deliver: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    if messaging_paused {
        info!("reply not sent (messaging paused)");
        return Ok(());
    }
    deliver(msg)
}

/// Send an away notification.
#[cfg(feature = "imessage")]
pub fn notify_away(turn: &TurnCompleted, trace_id: &str) -> Result<AwayNotification, String> {
    imessage::notify_away(turn, trace_id)
}

/// Send an away notification: a build without `imessage` has no away channel.
#[cfg(not(feature = "imessage"))]
pub fn notify_away(_turn: &TurnCompleted, _trace_id: &str) -> Result<AwayNotification, String> {
    Err(NO_IMESSAGE.into())
}

/// Send a message an agent asked for. The caller has already checked the messaging pause.
/// Unlike a notification the text is not summarised or split, and no duplicate check
/// applies: the agent chose to send it.
#[cfg(feature = "imessage")]
pub(crate) fn send_away_message(message: &AwayMessageRequested) -> Result<(), String> {
    let cfg = get_settings();
    let text = agent_message_text(&cfg.machine_name, &message.pane_label, &message.text);
    imessage::send_imessage(&text)
}

/// Send a message an agent asked for: a build without `imessage` has no away channel.
#[cfg(not(feature = "imessage"))]
pub(crate) fn send_away_message(_message: &AwayMessageRequested) -> Result<(), String> {
    Err(NO_IMESSAGE.into())
}

#[cfg(all(test, feature = "imessage"))]
mod tests {
    use super::{
        AwayMessageRequested, TurnCompleted, build_summary_prompt, drop_unmatched_questions,
        finish_summary, notify_away, send, send_away_message, send_unless_paused,
    };
    use crate::agent::domain::CompletionSummaryUpdate;
    use crate::settings::init_settings_for_test;

    const PHOTO_SUMMARY: &str = "I made an explainer page for the rsi harness. It is private to you. I also found why your omarchy machine suspended: a 30-minute idle timer fired during my background job. I have set the stay-awake switch for now. Do you approve the Harold spec change?";
    const PHOTO_REPLY: &str = "I made an explainer page for the rsi harness. It is private to you.\nI also found why your omarchy machine suspended: a 30-minute idle timer fired during my background job. I have set the stay-awake switch for now.";
    const BUILD_REPLY: &str =
        "The spec is ready. Do you want me to start the build now, or wait for the release?";

    #[test]
    fn keeps_question_the_reply_has() {
        let summary =
            "The spec is ready. Do you want me to start the build now, or wait for the release?";
        let (text, dropped) = drop_unmatched_questions(summary, BUILD_REPLY);
        assert_eq!(text, summary);
        assert!(dropped.is_empty());
    }

    #[test]
    fn keeps_paraphrased_question() {
        let summary = "The spec is ready. Should I start the build now?";
        let (text, dropped) = drop_unmatched_questions(summary, BUILD_REPLY);
        assert_eq!(text, summary);
        assert!(dropped.is_empty());
    }

    #[test]
    fn removes_unmatched_question_keeps_rest() {
        let (text, dropped) = drop_unmatched_questions(
            "The spec is ready. Do you approve the merge today?",
            "The spec is ready.",
        );
        assert_eq!(text, "The spec is ready.");
        assert_eq!(dropped.len(), 1);
        assert_eq!(
            dropped[0].chars,
            "Do you approve the merge today?".chars().count()
        );
    }

    #[test]
    fn only_unmatched_question_returns_empty() {
        let (text, dropped) =
            drop_unmatched_questions("Do you approve the merge today?", "The spec is ready.");
        assert_eq!(text, "");
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn no_question_unchanged() {
        let (text, dropped) = drop_unmatched_questions("The spec is ready.", "The spec is ready.");
        assert_eq!(text, "The spec is ready.");
        assert!(dropped.is_empty());
    }

    #[test]
    fn photo_case_drops_approve_question() {
        let (text, dropped) = drop_unmatched_questions(PHOTO_SUMMARY, PHOTO_REPLY);
        assert_eq!(
            text,
            "I made an explainer page for the rsi harness. It is private to you. I also found why your omarchy machine suspended: a 30-minute idle timer fired during my background job. I have set the stay-awake switch for now."
        );
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn question_with_no_content_words_is_unmatched() {
        let (text, dropped) = drop_unmatched_questions("It is done. Is it ok?", BUILD_REPLY);
        assert_eq!(text, "It is done.");
        assert_eq!(dropped[0].matched_share, 0);
    }

    /// Log text, kept by a subscriber for the test.
    #[derive(Clone, Default)]
    struct LogText(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogText {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogText {
        type Writer = LogText;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_log<T>(run: impl FnOnce() -> T) -> (T, String) {
        let log = LogText::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, run);
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        (result, text)
    }

    #[test]
    fn finish_summary_falls_back_to_the_whole_reply() {
        let (text, log) = capture_log(|| {
            finish_summary("Do you approve the merge today?", "The spec\nis ready.")
        });
        assert_eq!(text, "The spec is ready.");
        assert!(log.contains("fell_back=true"), "{log}");
    }

    #[test]
    fn finish_summary_logs_once_per_removed_sentence_without_text() {
        let (text, log) = capture_log(|| {
            finish_summary(
                "The spec is ready. Do you approve the merge today? Should anyone review secretword now?",
                "The spec is ready.",
            )
        });
        assert_eq!(text, "The spec is ready.");
        assert_eq!(log.matches("summary question dropped").count(), 2, "{log}");
        assert!(log.contains("reason=\"question_not_in_reply\""), "{log}");
        assert!(log.contains("chars=31"), "{log}");
        assert!(log.contains("matched_share=0"), "{log}");
        assert!(log.contains("fell_back=false"), "{log}");
        assert!(!log.contains("approve"), "{log}");
        assert!(!log.contains("secretword"), "{log}");
    }

    #[test]
    fn finish_summary_logs_nothing_when_no_question_is_removed() {
        let (text, log) =
            capture_log(|| finish_summary("The spec is ready.", "The spec is ready."));
        assert_eq!(text, "The spec is ready.");
        assert!(!log.contains("summary question dropped"), "{log}");
    }

    #[test]
    fn prompt_forbids_new_questions() {
        let prompt = build_summary_prompt("p", "m");
        assert!(
            prompt.contains(
                "Add no question or request that the assistant's reply does not contain."
            )
        );
        assert!(prompt.contains(
            "Use the user's words only to understand the reply. Never answer them or ask them anything."
        ));
    }

    #[test]
    fn paused_send_reports_success_without_delivering() {
        let mut delivered = Vec::new();
        let result = send_unless_paused("✓ Delivered to [harold:0.1]", true, |msg| {
            delivered.push(msg.to_string());
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert!(delivered.is_empty());
    }

    #[test]
    fn running_send_delivers_and_returns_the_channel_result() {
        let mut delivered = Vec::new();
        let result = send_unless_paused("hello", false, |msg| {
            delivered.push(msg.to_string());
            Err("channel down".to_string())
        });

        assert_eq!(result, Err("channel down".to_string()));
        assert_eq!(delivered, ["hello"]);
    }

    const NOT_CONFIGURED: &str = "iMessage recipient is not configured";

    fn turn() -> TurnCompleted {
        TurnCompleted {
            pane_id: "%1".into(),
            pane_label: "pods:0.1".into(),
            last_user_prompt: "do it".into(),
            assistant_message: "done".into(),
            main_context: String::new(),
            agent_incarnation: None,
            work_summary: CompletionSummaryUpdate::Unchanged,
        }
    }

    // The default settings have no iMessage recipient, so a call that reaches iMessage
    // answers that it is not configured, without sending anything.
    #[test]
    fn send_reaches_imessage() {
        init_settings_for_test();
        assert_eq!(send("hello"), Err(NOT_CONFIGURED.to_string()));
    }

    #[test]
    fn notify_away_reaches_imessage() {
        init_settings_for_test();
        assert_eq!(
            notify_away(&turn(), "trace").err(),
            Some(NOT_CONFIGURED.to_string())
        );
    }

    #[test]
    fn send_away_message_reaches_imessage() {
        init_settings_for_test();
        let message = AwayMessageRequested {
            pane_id: "%1".into(),
            pane_label: "pods:0.1".into(),
            text: "hi".into(),
        };
        assert_eq!(send_away_message(&message), Err(NOT_CONFIGURED.to_string()));
    }
}
