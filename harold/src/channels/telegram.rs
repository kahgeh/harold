use std::sync::Arc;
use std::sync::OnceLock;

use reqwest::blocking::Client;
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{Instrument, info, info_span, warn};

use super::{AwayNotification, split_body, summarise_for_notification};
use crate::inbound::AgentAddress;
use crate::settings::get_settings;
use crate::store::{HaroldStore, InboundMessage, TurnCompleted, append_inbound_message};

// ===========================================================================
// Sending
// ===========================================================================

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest blocking client")
    })
}

fn bot_url(token: &str, method: &str) -> String {
    format!("https://api.telegram.org/bot{token}/{method}")
}

fn sanitized_request_error(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

fn bounded_response_body(body: String) -> String {
    body.chars().take(200).collect()
}

/// Send a plain text message to the configured Telegram chat.
/// Must be called from a `spawn_blocking` context.
pub(crate) fn send_telegram(msg: &str) -> Result<(), String> {
    let cfg = get_settings();
    let Some(token) = cfg.telegram.bot_token.as_deref() else {
        warn!("telegram: bot_token not configured");
        return Err("telegram bot_token is not configured".into());
    };
    let Some(chat_id) = cfg.telegram.chat_id else {
        warn!("telegram: chat_id not configured");
        return Err("telegram chat_id is not configured".into());
    };

    info!(msg, "sending Telegram message");
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": msg,
    });

    let res = client()
        .post(bot_url(token, "sendMessage"))
        .json(&body)
        .send();

    match res {
        Ok(r) if r.status().is_success() => {
            info!("Telegram message sent");
            Ok(())
        }
        Ok(r) => {
            let status = r.status();
            let body = bounded_response_body(r.text().unwrap_or_default());
            warn!(
                status = %status,
                body,
                "Telegram sendMessage failed"
            );
            Err(format!("Telegram sendMessage returned {status}: {body}"))
        }
        Err(e) => {
            let error = sanitized_request_error(e);
            warn!(error, "Telegram sendMessage request error");
            Err(format!("Telegram sendMessage request failed: {error}"))
        }
    }
}

/// Send an OGG Opus voice message to the configured Telegram chat.
/// Must be called from a `spawn_blocking` context.
#[allow(dead_code)]
pub(crate) fn send_voice(ogg_path: &str, caption: &str) {
    let cfg = get_settings();
    let Some(token) = cfg.telegram.bot_token.as_deref() else {
        warn!("telegram: bot_token not configured");
        return;
    };
    let Some(chat_id) = cfg.telegram.chat_id else {
        warn!("telegram: chat_id not configured");
        return;
    };

    info!(path = ogg_path, "sending Telegram voice message");
    let file_bytes = match std::fs::read(ogg_path) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, path = ogg_path, "failed to read voice file");
            return;
        }
    };

    let file_name = std::path::Path::new(ogg_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("voice.ogg")
        .to_string();

    let part = reqwest::blocking::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str("audio/ogg")
        .expect("valid mime");

    let form = reqwest::blocking::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .text("caption", caption.to_string())
        .part("voice", part);

    let res = client()
        .post(bot_url(token, "sendVoice"))
        .multipart(form)
        .send();

    match res {
        Ok(r) if r.status().is_success() => {
            info!("Telegram voice message sent");
        }
        Ok(r) => {
            let status = r.status();
            let body = bounded_response_body(r.text().unwrap_or_default());
            warn!(
                status = %status,
                body,
                "Telegram sendVoice failed"
            );
        }
        Err(e) => {
            let error = sanitized_request_error(e);
            warn!(error, "Telegram sendVoice request error");
        }
    }
}

// ---------------------------------------------------------------------------
// Away notification via Telegram — returns the source agent address
// ---------------------------------------------------------------------------

pub(crate) fn notify_away(
    turn: &TurnCompleted,
    _trace_id: &str,
) -> Result<AwayNotification, String> {
    let body = summarise_for_notification(&turn.assistant_message, &turn.last_user_prompt);

    let (main_body, question) = split_body(&body);
    let message = format!(
        "🤖 [{}] {} ({})",
        turn.pane_label,
        main_body.trim(),
        turn.main_context
    );

    send_telegram(&message)?;
    info!("Telegram notification sent");

    if let Some(q) = question {
        send_telegram(&format!("🤖 {q}"))?;
        info!("Telegram question sent");
    }

    Ok(AwayNotification::Sent(AgentAddress::TmuxPane {
        pane_id: turn.pane_id.clone(),
        label: turn.pane_label.clone(),
    }))
}

// ===========================================================================
// Listening
// ===========================================================================

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

fn poll_updates(client: &Client, token: &str, offset: i64, timeout: u64) -> Option<Vec<Update>> {
    let url = format!("https://api.telegram.org/bot{token}/getUpdates");
    let res = client
        .get(&url)
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", timeout.to_string()),
        ])
        .send();

    match res {
        Ok(r) if r.status().is_success() => {
            let body: GetUpdatesResponse = match r.json() {
                Ok(b) => b,
                Err(e) => {
                    let error = sanitized_request_error(e);
                    warn!(
                        error,
                        "telegram_listener: failed to parse getUpdates response"
                    );
                    return None;
                }
            };
            if body.ok {
                Some(body.result)
            } else {
                warn!("telegram_listener: getUpdates returned ok=false");
                None
            }
        }
        Ok(r) => {
            let status = r.status();
            let text = bounded_response_body(r.text().unwrap_or_default());
            warn!(
                status = %status,
                body = %text,
                "telegram_listener: getUpdates HTTP error"
            );
            None
        }
        Err(e) => {
            let error = sanitized_request_error(e);
            warn!(error, "telegram_listener: getUpdates request error");
            None
        }
    }
}

pub(crate) async fn listen(store: Arc<HaroldStore>, mut shutdown: watch::Receiver<()>) {
    let cfg = get_settings();
    let Some(token) = cfg.telegram.bot_token.clone() else {
        warn!("telegram_listener: bot_token not configured, not starting");
        return;
    };
    let Some(expected_chat_id) = cfg.telegram.chat_id else {
        warn!("telegram_listener: chat_id not configured, not starting");
        return;
    };

    info!("Telegram listener started");

    // Create the HTTP client once — Client::clone is cheap (Arc internally).
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(35))
        .build()
        .expect("reqwest blocking client");

    // Drain any pre-existing updates so we don't replay old messages on startup.
    let mut offset: i64 = tokio::task::spawn_blocking({
        let client = client.clone();
        let token = token.clone();
        move || {
            if let Some(updates) = poll_updates(&client, &token, 0, 0) {
                updates.last().map(|u| u.update_id + 1).unwrap_or(0)
            } else {
                0
            }
        }
    })
    .await
    .unwrap_or(0);
    info!(initial_offset = offset, "Telegram listener drained backlog");

    loop {
        // Check shutdown before each poll cycle.
        if shutdown.has_changed().unwrap_or(true) {
            info!("Telegram listener shutting down");
            break;
        }

        let client = client.clone();
        let token_clone = token.clone();
        let current_offset = offset;

        let updates = tokio::task::spawn_blocking(move || {
            poll_updates(&client, &token_clone, current_offset, 30)
        })
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "telegram_listener: poll task panicked");
            None
        });

        let Some(updates) = updates else {
            // On error, wait a bit before retrying.
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("Telegram listener shutting down");
                    break;
                }
                () = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => continue,
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            let Some(msg) = update.message else {
                continue;
            };
            if msg.chat.id != expected_chat_id {
                continue;
            }
            let Some(text) = msg.text else {
                continue;
            };
            let text = text.trim().to_string();
            if text.is_empty() || text.starts_with('🤖') {
                continue;
            }

            let trace_id = uuid::Uuid::new_v4().to_string();
            let span = info_span!(
                "telegram_listener_inbound",
                trace_id = %trace_id,
                update_id = update.update_id,
            );

            async {
                info!("Telegram message received");
                match append_inbound_message(&store, &InboundMessage { text }).await {
                    Ok(()) => {}
                    Err(e) => warn!(error = %e, "failed to append InboundMessageReceived event"),
                }
            }
            .instrument(span)
            .await;
        }
    }
}
