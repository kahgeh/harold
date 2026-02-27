use std::process::Command;

use tracing::{info, warn};

use crate::route_reply::{AgentAddress, set_last_away_notification_source_agent};
use crate::settings::get_settings;
use crate::store::TurnCompleted;
use crate::util::sanitise_for_applescript;

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

fn regex_strip_think(text: &str) -> String {
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

fn build_short_summary(turn: &TurnCompleted) -> String {
    let system_prompt = "You are a notification assistant. Given a user's last request, \
         write ONLY a brief 3-8 word summary of what was completed. \
         Do not include any thinking, explanations, or extra text. \
         Output format: Just the summary message.";
    let prompt = format!(
        "User's last request: {}\n\nWrite a 3-8 word summary of what was done:",
        turn.last_user_prompt.chars().take(500).collect::<String>(),
    );
    run_local_model(system_prompt, &prompt, 20).unwrap_or_else(|| "Work complete".into())
}

pub fn notify_at_desk(turn: &TurnCompleted, _trace_id: &str) {
    let summary = build_short_summary(turn);
    let message = format!(
        "{} on {} and waiting for further instructions",
        summary, turn.main_context
    );
    let tts = &get_settings().tts;
    let mut cmd = Command::new(&tts.command);
    if let Some(extra_args) = &tts.args {
        cmd.args(extra_args);
    }
    if let Some(voice) = &tts.voice {
        cmd.args(["-v", voice]);
    }
    match cmd.arg(&message).status() {
        Ok(_) => info!("TTS notification sent"),
        Err(e) => warn!(error = %e, "TTS failed"),
    }
}

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

fn send_raw_imessage(text: &str, recipient: &str) {
    info!(msg = %text, "sending iMessage notification");
    let text = format!("🤖 {text}");
    let safe_text = sanitise_for_applescript(&text);
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

pub fn notify_away(turn: &TurnCompleted, trace_id: &str) {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        warn!("iMessage recipient not configured");
        return;
    };
    let body: String = turn
        .assistant_message
        .chars()
        .take(280)
        .collect::<String>()
        .replace('\n', " ");

    let (main_body, question) = split_body(&body);
    let short_id: String = trace_id.chars().take(8).collect();
    let message = format!(
        "[{}] {} ({}) [{}]",
        turn.pane_label,
        main_body.trim(),
        turn.main_context,
        short_id
    );

    let is_duplicate = cfg
        .imessage
        .handle_ids
        .first()
        .and_then(|&id| last_outgoing_text(id))
        .is_some_and(|last| last.trim().trim_start_matches("🤖").trim() == message.trim());
    if is_duplicate {
        info!("iMessage skipped (duplicate)");
        return;
    }

    send_raw_imessage(&message, recipient);
    info!("iMessage notification sent");
    set_last_away_notification_source_agent(AgentAddress::TmuxPane {
        pane_id: turn.pane_id.clone(),
        label: turn.pane_label.clone(),
    });

    if let Some(q) = question {
        send_raw_imessage(q, recipient);
        info!("iMessage question sent");
    }
}

fn active_tmux_session() -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-l", "-p", "#{session_name}"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn pane_session(pane_id: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{session_name}"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub fn notify(turn: &TurnCompleted, trace_id: &str) {
    let cfg = get_settings();

    if cfg.notify.skip_if_session_active
        && let (Some(active_session), Some(pane_session)) =
            (active_tmux_session(), pane_session(&turn.pane_id))
        && active_session == pane_session
    {
        info!("notification skipped (session is active)");
        return;
    }

    if is_screen_locked() {
        notify_away(turn, trace_id);
        return;
    }
    notify_at_desk(turn, trace_id);
}

#[cfg(test)]
#[path = "notify_test.rs"]
mod tests;
