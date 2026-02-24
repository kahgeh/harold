use std::process::Command;

use tracing::{info, warn};

use crate::settings::get_settings;
use crate::store::TurnCompleted;

pub fn is_screen_locked() -> bool {
    let result = Command::new("bash")
        .args([
            "-c",
            "ioreg -n Root -d1 -a | plutil -extract IOConsoleLocked raw -",
        ])
        .output();

    match result {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == "true",
        Err(_) => false,
    }
}

fn ai_cli_env() -> Vec<(String, String)> {
    // Allowlist: only forward variables the AI CLI actually needs.
    let allowed = [
        "PATH",
        "HOME",
        "ANTHROPIC_API_KEY",
        "TMPDIR",
        "LANG",
        "LC_ALL",
    ];
    std::env::vars()
        .filter(|(k, _)| allowed.contains(&k.as_str()))
        .collect()
}

fn run_ai_cli(prompt: &str) -> Option<String> {
    let cfg = get_settings();
    let cli = cfg.ai.cli_path.as_deref()?;

    let out = Command::new(cli)
        .args([
            "-p",
            prompt,
            "--model",
            "haiku",
            "--max-turns",
            "1",
            "--settings",
            r#"{"disableAllHooks":true}"#,
        ])
        .envs(ai_cli_env())
        .output()
        .ok()?;

    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() { None } else { Some(text) }
    } else {
        None
    }
}

fn build_short_summary(turn: &TurnCompleted) -> String {
    let prompt = format!(
        "Given this coding task completion, write ONLY a brief 3-8 word summary of what was done.\n\
         User asked: {}\n\
         Assistant replied: {}\n\
         Summary:",
        turn.last_user_prompt.chars().take(300).collect::<String>(),
        turn.assistant_message.chars().take(300).collect::<String>(),
    );
    run_ai_cli(&prompt).unwrap_or_else(|| "Work complete".into())
}

fn build_detailed_summary(turn: &TurnCompleted) -> Option<String> {
    let prompt = format!(
        "Write a concise phone notification (under 300 chars, plain text, no markdown) covering:\n\
         1. What was asked (1 short phrase)\n\
         2. What was done or current status (1-2 sentences)\n\
         3. Action needed from user, if any\n\n\
         User asked: {}\n\
         Assistant replied: {}\n\
         Notification:",
        turn.last_user_prompt.chars().take(500).collect::<String>(),
        turn.assistant_message
            .chars()
            .take(2000)
            .collect::<String>(),
    );
    run_ai_cli(&prompt)
}

pub fn notify_at_desk(turn: &TurnCompleted) {
    let summary = build_short_summary(turn);
    let message = format!(
        "{} on {} and waiting for further instructions",
        summary, turn.main_context
    );
    let cmd = &get_settings().tts.command;
    match Command::new(cmd).arg(&message).status() {
        Ok(_) => info!("TTS notification sent"),
        Err(e) => warn!(error = %e, "TTS failed"),
    }
}

fn split_body(body: &str) -> (&str, Option<&str>) {
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

fn sanitise_for_applescript(text: &str) -> String {
    // Strip characters that can break or escape an AppleScript string literal
    // passed via `osascript -e`. Newlines end the statement; ¬ is the
    // AppleScript line-continuation character; control chars are noise.
    text.chars()
        .filter(|c| *c != '\n' && *c != '\r' && *c != '¬' && !c.is_control())
        .collect()
}

fn send_raw_imessage(text: &str, recipient: &str) {
    let safe_text = sanitise_for_applescript(text);
    let safe_recipient = sanitise_for_applescript(recipient);
    let escaped = safe_text.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_recipient = safe_recipient.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"Messages\" to send \"{escaped}\" to buddy \"{escaped_recipient}\""
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
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
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    }
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

pub fn notify_away(turn: &TurnCompleted) {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        warn!("iMessage recipient not configured");
        return;
    };
    let handle_id = cfg.imessage.handle_id.unwrap_or(0);

    // Use detailed summary; fall back to truncated assistant message if AI is unavailable.
    let body = build_detailed_summary(turn).unwrap_or_else(|| {
        turn.assistant_message
            .chars()
            .take(280)
            .collect::<String>()
            .replace('\n', " ")
    });

    let (main_body, question) = split_body(&body);
    let message = format!(
        "[{}] {} ({})",
        turn.pane_label, main_body, turn.main_context
    );

    if handle_id > 0
        && let Some(last) = last_outgoing_text(handle_id)
        && last.trim() == message.trim()
    {
        info!("iMessage skipped (duplicate)");
        return;
    }

    send_raw_imessage(&message, recipient);
    info!("iMessage notification sent");

    if let Some(q) = question {
        send_raw_imessage(q, recipient);
        info!("iMessage question sent");
    }
}

pub fn notify(turn: &TurnCompleted) {
    if is_screen_locked() {
        notify_away(turn);
    } else {
        notify_at_desk(turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // No preceding sentence — falls back to returning the full body
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
