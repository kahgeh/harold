pub(crate) mod imessage;

use std::process::Command;
use std::sync::Arc;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::inbound::AgentAddress;
use crate::inbound::ownership::agent_message_text;
use crate::settings::get_settings;
use crate::store::{AwayMessageRequested, HaroldStore, TurnCompleted};
use crate::util::ai_cli_env;

pub(crate) enum AwayNotification {
    Sent(AgentAddress),
    Skipped,
}

// ---------------------------------------------------------------------------
// Shared utilities — used by the iMessage channel
// ---------------------------------------------------------------------------

/// Extract a trailing question sentence from the assistant message body.
pub(crate) fn split_body(body: &str) -> (&str, Option<&str>) {
    // byte-index arithmetic below is safe only because '?' and '.' are single-byte ASCII chars.
    // rfind on char guarantees char-boundary alignment for q_pos.
    // sentence_start is q_pos's preceding '.' position + 1 (one byte past ASCII '.'), also safe.
    if let Some(q_pos) = body.rfind('?') {
        let sentence_start = body[..q_pos].rfind('.').map_or(0, |i| i + 1);
        let question = body[sentence_start..=q_pos].trim();
        let main = body[..sentence_start].trim();
        if !main.is_empty() && !question.is_empty() {
            return (main, Some(question));
        }
    }
    (body.trim(), None)
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
    let prompt = format!(
        "You are writing a phone notification summary.\n\n\
         USER ASKED:\n<prompt>\n{safe_prompt}\n</prompt>\n\n\
         ASSISTANT REPLIED:\n<message>\n{safe_msg}\n</message>\n\n\
         Write 2-3 plain sentences summarising what was done and the outcome.\n\
         Preserve any question the assistant asked.\n\
         No code, no markdown, no jargon. Keep it under 280 characters."
    );

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
                    summary.replace('\n', " ")
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

/// The text of a reply worth routing: trimmed, and neither empty nor a message Harold sent
/// itself. A member storing a forwarded reply applies it.
pub(crate) fn reply_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || imessage::is_marked_as_harold(text) {
        return None;
    }
    Some(text.to_string())
}

/// Start the inbound message listener.
pub async fn listen_for_inbound_messages(store: Arc<HaroldStore>, shutdown: watch::Receiver<()>) {
    imessage::listen(store, shutdown).await;
}

/// Send a plain message to the away device.
pub fn send(msg: &str) -> Result<(), String> {
    send_unless_paused(
        msg,
        crate::outbound::is_messaging_paused(),
        imessage::send_imessage,
    )
}

/// Hand `msg` to `deliver` unless messaging is paused. A paused send reports success:
/// the reply is dropped on purpose, so the caller must not retry it.
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
pub fn notify_away(turn: &TurnCompleted, trace_id: &str) -> Result<AwayNotification, String> {
    imessage::notify_away(turn, trace_id)
}

/// Send a message an agent asked for. The caller has already checked the messaging pause.
/// Unlike a notification the text is not summarised or split, and no duplicate check
/// applies: the agent chose to send it.
pub(crate) fn send_away_message(message: &AwayMessageRequested) -> Result<(), String> {
    let cfg = get_settings();
    let text = agent_message_text(&cfg.machine_name, &message.pane_label, &message.text);
    imessage::send_imessage(&text)
}

/// Send a spoken line to the away device as a voice note. The caller has already checked
/// the messaging pause.
pub(crate) fn send_spoken_line(line: &str) -> Result<(), String> {
    let cfg = get_settings();
    // Checked before the note is made, so a retry does not rerun the model for nothing.
    if cfg.imessage.recipient.is_none() {
        return Err("iMessage recipient is not configured".into());
    }
    crate::outbound::voice_note::send_voice_note_in(
        &crate::outbound::voice_note::notes_parent(),
        std::time::SystemTime::now(),
        &cfg.tts,
        line,
        &crate::outbound::voice_note::SystemRunner,
        imessage::send_attachment,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AwayMessageRequested, TurnCompleted, notify_away, reply_text, send, send_away_message,
        send_spoken_line, send_unless_paused,
    };
    use crate::agent::domain::CompletionSummaryUpdate;
    use crate::settings::init_settings_for_test;

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

    #[test]
    fn send_spoken_line_reaches_imessage() {
        init_settings_for_test();
        assert_eq!(
            send_spoken_line("good morning"),
            Err(NOT_CONFIGURED.to_string())
        );
    }

    #[test]
    fn reply_text_trims_and_skips_empty_and_harold_marked_text() {
        assert_eq!(reply_text("  yes  "), Some("yes".to_string()));
        assert_eq!(reply_text("   "), None);
        assert_eq!(reply_text("🤖 Delivered"), None);
    }
}
