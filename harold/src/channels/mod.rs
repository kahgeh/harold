pub(crate) mod imessage;
pub(crate) mod telegram;

pub(crate) use telegram::reply_text;

use std::process::Command;
use std::sync::Arc;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::inbound::AgentAddress;
use crate::inbound::ownership::agent_message_text;
use crate::settings::{PeerRole, Settings, get_settings};
use crate::store::{AwayMessageRequested, HaroldStore, TurnCompleted};
use crate::util::ai_cli_env;

pub(crate) enum AwayNotification {
    Sent(AgentAddress),
    Skipped,
}

// ---------------------------------------------------------------------------
// Shared utilities — used by both iMessage and Telegram channels
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

/// Cap `assistant_message` to 280 chars and flatten newlines into spaces.
pub(crate) fn truncate_body(assistant_message: &str) -> String {
    assistant_message
        .chars()
        .take(280)
        .collect::<String>()
        .replace('\n', " ")
}

/// Summarise `assistant_message` for notification delivery using the AI CLI.
/// Falls back to [`truncate_body`] if the CLI is not configured or fails.
pub(crate) fn summarise_for_notification(
    assistant_message: &str,
    last_user_prompt: &str,
) -> String {
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
                "summarise_for_notification: AI CLI failed, falling back to truncation"
            );
            truncate_body(assistant_message)
        }
        Err(e) => {
            warn!(error = %e, "summarise_for_notification: failed to spawn AI CLI, falling back to truncation");
            truncate_body(assistant_message)
        }
    }
}

// ---------------------------------------------------------------------------
// Static dispatch — delegates to the configured away channel
// ---------------------------------------------------------------------------

/// Which listener reads replies on this Mac.
#[derive(Debug, PartialEq, Eq)]
enum InboundListener {
    Imessage,
    Telegram,
    /// A member on Telegram. Telegram lets one process read a bot's updates, and a second
    /// reader gets a conflict, so only the hub reads them. The hub forwards this Mac its
    /// replies.
    None,
}

fn inbound_listener(away_channel: &str, role: PeerRole) -> InboundListener {
    match (away_channel, role) {
        ("telegram", PeerRole::Member) => InboundListener::None,
        ("telegram", PeerRole::Hub) => InboundListener::Telegram,
        _ => InboundListener::Imessage,
    }
}

/// The listener `settings` call for.
fn inbound_listener_for(settings: &Settings) -> InboundListener {
    inbound_listener(&settings.notify.away_channel, settings.peers.role)
}

/// Start the inbound message listener for the configured away channel.
pub async fn listen_for_inbound_messages(store: Arc<HaroldStore>, shutdown: watch::Receiver<()>) {
    match inbound_listener_for(get_settings()) {
        InboundListener::Telegram => telegram::listen(store, shutdown).await,
        InboundListener::Imessage => imessage::listen(store, shutdown).await,
        InboundListener::None => {
            info!("Telegram listener not started: this Mac is a member and the hub reads replies");
        }
    }
}

/// Send a plain message through the configured away channel.
pub fn send(msg: &str) -> Result<(), String> {
    send_unless_paused(msg, crate::outbound::is_messaging_paused(), |msg| {
        let cfg = get_settings();
        match cfg.notify.away_channel.as_str() {
            "telegram" => telegram::send_telegram(msg),
            _ => imessage::send_imessage(msg),
        }
    })
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

/// Send an away notification through the configured away channel.
pub fn notify_away(turn: &TurnCompleted, trace_id: &str) -> Result<AwayNotification, String> {
    let cfg = get_settings();
    match cfg.notify.away_channel.as_str() {
        "telegram" => telegram::notify_away(turn, trace_id),
        _ => imessage::notify_away(turn, trace_id),
    }
}

/// Send a message an agent asked for through the configured away channel. The caller has
/// already checked the messaging pause. Unlike a notification the text is not summarised or
/// split, and no duplicate check applies: the agent chose to send it.
pub(crate) fn send_away_message(message: &AwayMessageRequested) -> Result<(), String> {
    let cfg = get_settings();
    let text = agent_message_text(&cfg.machine_name, &message.pane_label, &message.text);
    match cfg.notify.away_channel.as_str() {
        "telegram" => telegram::send_agent_message(
            &cfg.peers,
            &cfg.machine_name,
            &text,
            telegram::send_telegram,
        ),
        _ => imessage::send_imessage(&text),
    }
}

/// Send a spoken line to the away device: a voice note with iMessage, the line as text
/// with Telegram. The caller has already checked the messaging pause.
pub(crate) fn send_spoken_line(line: &str) -> Result<(), String> {
    let cfg = get_settings();
    match cfg.notify.away_channel.as_str() {
        "telegram" => telegram::send_spoken_line(line),
        _ => crate::outbound::voice_note::send_voice_note_in(
            &crate::outbound::voice_note::notes_parent(),
            std::time::SystemTime::now(),
            &cfg.tts,
            line,
            &crate::outbound::voice_note::SystemRunner,
            imessage::send_attachment,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{InboundListener, inbound_listener, inbound_listener_for, send_unless_paused};
    use crate::peers::settings_with;
    use crate::settings::PeerRole;

    const MEMBER: &str = r#"
        machine_name = "laptop"
        machines = ["studio", "laptop"]
        [peers]
        role = "member"
        [peers.hub]
        url = "http://studio.tailnet:50060"
        secret = "s3cret"
    "#;

    fn on(channel: &str, role_settings: &str) -> crate::settings::Settings {
        settings_with(&format!(
            "{role_settings}\n[notify]\naway_channel = \"{channel}\""
        ))
    }

    #[test]
    fn listener_follows_the_away_channel_and_role_of_the_settings_it_is_given() {
        let hub = "machine_name = \"studio\"";
        assert_eq!(
            inbound_listener_for(&on("telegram", MEMBER)),
            InboundListener::None
        );
        assert_eq!(
            inbound_listener_for(&on("telegram", hub)),
            InboundListener::Telegram
        );
        assert_eq!(
            inbound_listener_for(&on("imessage", MEMBER)),
            InboundListener::Imessage
        );
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

    #[test]
    fn telegram_member_starts_no_listener_because_the_hub_reads_the_bot() {
        assert_eq!(
            inbound_listener("telegram", PeerRole::Member),
            InboundListener::None
        );
        assert_eq!(
            inbound_listener("telegram", PeerRole::Hub),
            InboundListener::Telegram
        );
    }

    #[test]
    fn imessage_listener_starts_whatever_the_role() {
        for role in [PeerRole::Hub, PeerRole::Member] {
            assert_eq!(
                inbound_listener("imessage", role),
                InboundListener::Imessage
            );
        }
    }
}
