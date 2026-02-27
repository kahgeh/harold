use std::process::Command;

use tracing::{info, warn};

use crate::inbound::AgentAddress;
use crate::settings::get_settings;
use crate::store::TurnCompleted;
use crate::util::sanitise_for_applescript;

// ---------------------------------------------------------------------------
// iMessage helpers
// ---------------------------------------------------------------------------

/// Low-level iMessage send — delivers `text` as-is (no prefix) to `recipient`.
pub(crate) fn send_imessage_to(text: &str, recipient: &str) {
    let safe_text = sanitise_for_applescript(text);
    let safe_recipient = sanitise_for_applescript(recipient);
    let escaped = safe_text.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_recipient = safe_recipient.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"Messages\" to send \"{escaped}\" to buddy \"{escaped_recipient}\""
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

/// Send an iMessage notification with robot-emoji prefix.
fn send_raw_imessage(text: &str, recipient: &str) {
    info!(msg = %text, "sending iMessage notification");
    send_imessage_to(&format!("🤖 {text}"), recipient);
}

/// Send a plain iMessage (confirmation/error) to the configured recipient.
pub(crate) fn send_imessage(msg: &str) {
    info!(msg, "sending iMessage");
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        return;
    };
    send_imessage_to(msg, recipient);
}

fn query_chat_db_single(db_path: &str, sql: &str) -> Option<String> {
    // SQL is built only from i64-typed values (handle_id). String interpolation of
    // non-numeric values must never be added here — use the sqlite3 CLI's `-cmd` flag
    // or a native library if parameterised queries are ever needed.
    let out = Command::new("sqlite3")
        .arg(db_path)
        .arg(sql)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn last_outgoing_text(handle_id: i64) -> Option<String> {
    let db_path = get_settings().chat_db.resolved_path();
    // handle_id is i64 from settings — safe to interpolate.
    query_chat_db_single(
        &db_path,
        &format!(
            "SELECT text FROM message WHERE handle_id = {handle_id} AND is_from_me = 1 \
             ORDER BY ROWID DESC LIMIT 1;"
        ),
    )
}

// ---------------------------------------------------------------------------
// split_body — extract trailing question from assistant message
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Away notification via iMessage — returns the source agent address
// ---------------------------------------------------------------------------

pub fn notify_away(turn: &TurnCompleted, _trace_id: &str) -> Option<AgentAddress> {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        warn!("iMessage recipient not configured");
        return None;
    };
    let body: String = turn
        .assistant_message
        .chars()
        .take(280)
        .collect::<String>()
        .replace('\n', " ");

    let (main_body, question) = split_body(&body);
    let message = format!(
        "[{}] {} ({})",
        turn.pane_label,
        main_body.trim(),
        turn.main_context
    );

    let is_duplicate = cfg
        .imessage
        .handle_ids
        .first()
        .and_then(|&id| last_outgoing_text(id))
        .is_some_and(|last| last.trim().trim_start_matches("🤖").trim() == message.trim());
    if is_duplicate {
        info!("iMessage skipped (duplicate)");
        return None;
    }

    send_raw_imessage(&message, recipient);
    info!("iMessage notification sent");

    if let Some(q) = question {
        send_raw_imessage(q, recipient);
        info!("iMessage question sent");
    }

    Some(AgentAddress::TmuxPane {
        pane_id: turn.pane_id.clone(),
        label: turn.pane_label.clone(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::split_body;
    use crate::util::sanitise_for_applescript;

    #[test]
    fn split_body_no_question() {
        let (main, q) = split_body("Work is done. All good.");
        assert_eq!(main, "Work is done. All good.");
        assert_eq!(q, None);
    }

    #[test]
    fn split_body_trailing_question() {
        let (main, q) = split_body("Build succeeded. Should I deploy?");
        assert_eq!(main, "Build succeeded.");
        assert_eq!(q, Some("Should I deploy?"));
    }

    #[test]
    fn split_body_only_question() {
        let (main, q) = split_body("Should I deploy?");
        assert_eq!(main, "Should I deploy?");
        assert_eq!(q, None);
    }

    #[test]
    fn split_body_multiple_sentences_with_question() {
        let (main, q) = split_body("Done. Tests pass. Ready to merge. Shall I open a PR?");
        assert_eq!(main, "Done. Tests pass. Ready to merge.");
        assert_eq!(q, Some("Shall I open a PR?"));
    }

    #[test]
    fn sanitise_strips_newlines_and_continuation() {
        let result = sanitise_for_applescript("line1\nline2\r¬end");
        assert!(!result.contains('\n'));
        assert!(!result.contains('\r'));
        assert!(!result.contains('¬'));
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
    }
}
