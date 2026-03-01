use std::process::Command;

use tracing::{info, warn};

use crate::inbound::AgentAddress;
use crate::settings::get_settings;
use crate::store::TurnCompleted;
use crate::util::{ai_cli_env, sanitise_for_applescript};

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
// AI-powered summary for iMessage notifications
// ---------------------------------------------------------------------------

/// Cap `assistant_message` to 280 chars and flatten newlines into spaces.
fn truncate_body(assistant_message: &str) -> String {
    assistant_message
        .chars()
        .take(280)
        .collect::<String>()
        .replace('\n', " ")
}

/// Summarise `assistant_message` for iMessage delivery using the AI CLI.
/// Falls back to [`truncate_body`] if the CLI is not configured or fails.
fn summarise_for_imessage(assistant_message: &str, last_user_prompt: &str) -> String {
    let cfg = get_settings();
    let Some(cli) = cfg.ai.cli_path.as_deref() else {
        return truncate_body(assistant_message);
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
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .env_remove("CLAUDECODE")
        .envs(ai_cli_env())
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let summary = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if summary.is_empty() {
                truncate_body(assistant_message)
            } else {
                truncate_body(&summary)
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            warn!(
                status = %o.status,
                stderr = %stderr.chars().take(200).collect::<String>(),
                "summarise_for_imessage: AI CLI failed, falling back to truncation"
            );
            truncate_body(assistant_message)
        }
        Err(e) => {
            warn!(error = %e, "summarise_for_imessage: failed to spawn AI CLI, falling back to truncation");
            truncate_body(assistant_message)
        }
    }
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
    let body = summarise_for_imessage(&turn.assistant_message, &turn.last_user_prompt);

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
    fn truncate_body_caps_at_280_chars_and_flattens_newlines() {
        use super::truncate_body;

        let short = "Hello world.\nDone.";
        assert_eq!(truncate_body(short), "Hello world. Done.");

        let long: String = "x".repeat(300);
        let result = truncate_body(&long);
        assert_eq!(result.len(), 280);

        // Multi-byte: caps at 280 *characters*, not bytes.
        let emoji_long: String = "\u{1F600}".repeat(300);
        let result = truncate_body(&emoji_long);
        assert_eq!(result.chars().count(), 280);
        assert!(result.len() > 280);
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
