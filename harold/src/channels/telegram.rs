use std::sync::Arc;
use std::sync::OnceLock;

use reqwest::blocking::Client;
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{Instrument, info, info_span, warn};

use super::{AwayNotification, split_body, summarise_for_notification};
use crate::inbound::AgentAddress;
use crate::inbound::ownership::{
    Ownership, decide_ownership, error_text, latest_notifier, notification_text,
    record_notification, same_machine,
};
use crate::outbound::is_messaging_paused;
use crate::peers::{forward_reply, report_notification_blocking};
use crate::settings::{PeerMember, PeerRole, PeersSettings, Settings, get_settings};
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

/// Marks text as written by Harold.
const HAROLD_MARKER: char = '🤖';

/// `msg` as it is sent. Every message goes out marked here, in the one place that builds
/// a send, so the listener skips it wherever it is read, and a notification is found by
/// its `🤖 [machine/pane]` start.
fn marked(msg: &str) -> String {
    format!("{HAROLD_MARKER} {msg}")
}

/// The `sendMessage` request body for `msg`, marked as written by Harold.
fn send_message_body(chat_id: i64, msg: &str) -> serde_json::Value {
    serde_json::json!({
        "chat_id": chat_id,
        "text": marked(msg),
    })
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
    let body = send_message_body(chat_id, msg);

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

/// The notification text for `turn`, naming the Mac that sends it.
fn notification_message(machine: &str, turn: &TurnCompleted, main_body: &str) -> String {
    notification_text(machine, &turn.pane_label, main_body, &turn.main_context)
}

/// Make this notification known to the hub's record of who notified last, before it is
/// sent. The hub records itself. A member reports to the hub; when the report fails the
/// notification is still sent, and the hub's record is stale until the next notification.
fn announce_notification(peers: &PeersSettings, machine: &str) {
    match (peers.role, &peers.hub) {
        (PeerRole::Hub, _) => record_notification(machine),
        (PeerRole::Member, Some(hub)) => {
            if let Err(error) = report_notification_blocking(hub, machine) {
                warn!(
                    error,
                    "notification not reported to the hub; sending it anyway"
                );
            }
        }
        // `validate` refuses this configuration before Harold starts.
        (PeerRole::Member, None) => warn!("peers.hub not configured; notification not reported"),
    }
}

/// Announce the notification for `turn`, then send it and its question through `send`.
fn send_notification(
    turn: &TurnCompleted,
    machine: &str,
    peers: &PeersSettings,
    body: &str,
    send: impl Fn(&str) -> Result<(), String>,
) -> Result<AwayNotification, String> {
    let (main_body, question) = split_body(body);
    let message = notification_message(machine, turn, main_body);

    announce_notification(peers, machine);
    send(&message)?;
    info!("Telegram notification sent");

    if let Some(q) = question {
        send(q)?;
        info!("Telegram question sent");
    }

    Ok(AwayNotification::Sent(AgentAddress::TmuxPane {
        pane_id: turn.pane_id.clone(),
        label: turn.pane_label.clone(),
    }))
}

/// Must be called from a `spawn_blocking` context.
pub(crate) fn notify_away(
    turn: &TurnCompleted,
    _trace_id: &str,
) -> Result<AwayNotification, String> {
    let body = summarise_for_notification(&turn.assistant_message, &turn.last_user_prompt);
    notify_with(get_settings(), turn, &body, send_telegram)
}

/// `send_notification` for the Mac `settings` describe: its name, and its role among the
/// Macs.
fn notify_with(
    settings: &Settings,
    turn: &TurnCompleted,
    body: &str,
    send: impl Fn(&str) -> Result<(), String>,
) -> Result<AwayNotification, String> {
    send_notification(turn, &settings.machine_name, &settings.peers, body, send)
}

/// Send a message an agent asked for. It is announced first, as a notification is, so the
/// hub knows which Mac an untagged reply belongs to.
pub(crate) fn send_agent_message(
    peers: &PeersSettings,
    machine: &str,
    text: &str,
    send: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    announce_notification(peers, machine);
    send(text)?;
    info!("Telegram agent message sent");
    Ok(())
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

/// The text of an update that should be routed, if any. While messaging is paused nothing
/// is: the caller has already moved its offset past the update, so it is not replayed.
fn routable_text(
    message: Option<Message>,
    expected_chat_id: i64,
    messaging_paused: bool,
) -> Option<String> {
    if messaging_paused {
        return None;
    }
    let msg = message?;
    if msg.chat.id != expected_chat_id {
        return None;
    }
    reply_text(&msg.text?)
}

/// The text of a reply worth routing: trimmed, and neither empty nor a message Harold sent
/// itself. The Telegram listener and a member storing a forwarded reply both apply it.
pub(crate) fn reply_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.starts_with(HAROLD_MARKER) {
        return None;
    }
    Some(text.to_string())
}

/// What the hub needs around it to place a reply: who the Macs are, which one notified
/// last, and a way to tell the user. A seam so forwarding is tested without Telegram.
trait HubEnv {
    fn machine(&self) -> &str;
    fn known_machines(&self) -> Vec<String>;
    fn latest_notifier(&self) -> Option<String>;
    fn members(&self) -> &[PeerMember];
    async fn tell_user(&self, text: String);
}

/// The hub of the Mac `settings` describe, telling the user through the away channel.
struct LiveHub<'a> {
    settings: &'a Settings,
    /// The Mac that notified last, from the record of this process.
    latest_notifier: fn() -> Option<String>,
    /// How the user is told: the configured away channel.
    send: fn(&str) -> Result<(), String>,
}

impl<'a> LiveHub<'a> {
    fn new(settings: &'a Settings) -> Self {
        Self {
            settings,
            latest_notifier,
            send: super::send,
        }
    }
}

impl HubEnv for LiveHub<'_> {
    fn machine(&self) -> &str {
        &self.settings.machine_name
    }

    fn known_machines(&self) -> Vec<String> {
        self.settings.machines.clone()
    }

    fn latest_notifier(&self) -> Option<String> {
        (self.latest_notifier)()
    }

    fn members(&self) -> &[PeerMember] {
        &self.settings.peers.members
    }

    async fn tell_user(&self, text: String) {
        tell_user_with(text, self.send).await;
    }
}

/// Send `text` with `send`, off the async workers because the send blocks. A failed send
/// is logged and nothing more: the reply it answers has already been dealt with.
async fn tell_user_with(
    text: String,
    send: impl FnOnce(&str) -> Result<(), String> + Send + 'static,
) {
    match tokio::task::spawn_blocking(move || send(&text)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(error, "telegram_listener: could not tell the user"),
        Err(error) => warn!(error = %error, "telegram_listener: send task panicked"),
    }
}

/// Where a reply read from Telegram goes.
#[derive(Debug, PartialEq, Eq)]
enum ReplyPlace {
    /// This Mac routes it.
    Here,
    /// The member with this machine name routes it.
    Member(String),
}

/// Decide which Mac owns `text`. The hub is the only Mac that reads the reply, so it
/// decides for all of them, against the latest notifier at this moment.
fn place_reply(text: &str, env: &impl HubEnv) -> ReplyPlace {
    let ownership = decide_ownership(
        text,
        false,
        env.machine(),
        &env.known_machines(),
        env.latest_notifier().as_deref(),
    );
    match ownership {
        Ownership::Owned { .. } => ReplyPlace::Here,
        Ownership::NotOwned { owner } => ReplyPlace::Member(owner),
    }
}

/// Keep `text` for this Mac or forward it to the member that owns it. A reply that cannot
/// be forwarded is not routed here: the user is told once which Mac could not be reached.
async fn handle_reply(store: &HaroldStore, env: &impl HubEnv, text: String) {
    let owner = match place_reply(&text, env) {
        ReplyPlace::Member(owner) => owner,
        ReplyPlace::Here => {
            // forwarded = true records that ownership is decided. Without it the event
            // handler would decide again, and a member's report that arrived in between
            // would make it drop a reply no other Mac ever sees.
            let message = InboundMessage {
                text,
                forwarded: true,
            };
            if let Err(e) = append_inbound_message(store, &message).await {
                warn!(error = %e, "failed to append InboundMessageReceived event");
            }
            return;
        }
    };

    let member = env
        .members()
        .iter()
        .find(|member| same_machine(&member.name, &owner));
    let forwarded = match member {
        Some(member) => forward_reply(member, &text).await,
        None => Err("not in peers.members".to_string()),
    };
    match forwarded {
        Ok(()) => info!(owner, "reply forwarded"),
        Err(error) => {
            warn!(
                owner,
                error, "telegram_listener: could not forward the reply"
            );
            let notice = error_text(env.machine(), &format!("could not reach {owner}"));
            env.tell_user(notice).await;
        }
    }
}

/// Handle one batch of updates. Returns the offset to poll from next: one past the last
/// update, or `offset` unchanged for an empty batch.
async fn handle_updates(
    store: &HaroldStore,
    env: &impl HubEnv,
    updates: Vec<Update>,
    mut offset: i64,
    expected_chat_id: i64,
    messaging_paused: impl Fn() -> bool,
) -> i64 {
    for update in updates {
        offset = update.update_id + 1;

        let Some(text) = routable_text(update.message, expected_chat_id, messaging_paused()) else {
            continue;
        };

        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!(
            "telegram_listener_inbound",
            trace_id = %trace_id,
            update_id = update.update_id,
        );

        async {
            info!("Telegram message received");
            handle_reply(store, env, text).await;
        }
        .instrument(span)
        .await;
    }
    offset
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

/// Where the listener reads whether messaging is paused: the switch the projector keeps.
fn pause_check() -> fn() -> bool {
    is_messaging_paused
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

        offset = handle_updates(
            &store,
            &LiveHub::new(cfg),
            updates,
            offset,
            expected_chat_id,
            pause_check(),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Chat, HubEnv, LiveHub, Message, ReplyPlace, Update, handle_updates, marked,
        notification_message, notify_with, pause_check, place_reply, routable_text,
        send_agent_message, send_message_body, send_notification, tell_user_with,
    };
    use crate::inbound::ownership::{delivered_text, error_text, notification_text, parse_marker};
    use crate::store::TurnCompleted;

    fn message(chat_id: i64, text: &str) -> Option<Message> {
        Some(Message {
            chat: Chat { id: chat_id },
            text: Some(text.into()),
        })
    }

    #[test]
    fn running_listener_routes_text_from_the_configured_chat() {
        assert_eq!(
            routable_text(message(7, " carry on "), 7, false),
            Some("carry on".to_string())
        );
        assert_eq!(routable_text(message(8, "carry on"), 7, false), None);
        assert_eq!(routable_text(message(7, "🤖 done"), 7, false), None);
        assert_eq!(routable_text(message(7, "  "), 7, false), None);
        assert_eq!(routable_text(None, 7, false), None);
    }

    #[test]
    fn paused_listener_discards_every_update() {
        assert_eq!(routable_text(message(7, "carry on"), 7, true), None);
    }

    #[test]
    fn every_text_is_sent_marked_as_harold() {
        assert_eq!(
            marked(&delivered_text("studio", "alir-app main:0.1")),
            "🤖 ✓ Delivered to [studio/alir-app main:0.1]"
        );
        assert_eq!(
            marked(&error_text("studio", "No active agent sessions found.")),
            "🤖 [studio] No active agent sessions found."
        );
        assert_eq!(marked("Should I deploy?"), "🤖 Should I deploy?");
    }

    #[test]
    fn notification_is_sent_in_the_form_the_marker_parser_reads() {
        let sent = marked(&notification_text(
            "studio",
            "alir-app main:0.1",
            "Tests pass.",
            "alir",
        ));

        assert_eq!(sent, "🤖 [studio/alir-app main:0.1] Tests pass. (alir)");
        assert_eq!(
            parse_marker(&sent).map(|marker| marker.machine),
            Some("studio")
        );
    }

    #[test]
    fn listener_skips_every_text_harold_sends() {
        for text in [
            delivered_text("studio", "alir-app main:0.1"),
            error_text("studio", "No active agent sessions found."),
            notification_text("studio", "alir-app main:0.1", "Tests pass.", "alir"),
        ] {
            assert_eq!(
                routable_text(message(7, &marked(&text)), 7, false),
                None,
                "{text:?}"
            );
        }
    }

    #[test]
    fn text_sent_to_telegram_is_marked_as_harold() {
        let body = send_message_body(7, "✓ Delivered to [studio/alir-app main:0.1]");

        assert_eq!(body["chat_id"], 7);
        assert_eq!(body["text"], "🤖 ✓ Delivered to [studio/alir-app main:0.1]");
    }

    #[test]
    fn notification_names_the_machine_and_the_pane() {
        assert_eq!(
            notification_message("studio", &turn(), " Tests pass. "),
            "[studio/alir-app main:0.1] Tests pass. (alir)"
        );
    }

    use std::sync::Mutex;

    use crate::inbound::ownership::recorded_notifications;
    use crate::peers::PeerEndpoint;
    use crate::settings::{PeerHub, PeerMember, PeerRole, PeersSettings};
    use crate::tests::{closed_port_url, start_peer_server};

    fn turn() -> TurnCompleted {
        TurnCompleted {
            pane_id: "%1".into(),
            pane_label: "alir-app main:0.1".into(),
            last_user_prompt: "prompt".into(),
            assistant_message: "message".into(),
            main_context: "alir".into(),
            agent_incarnation: None,
            work_summary: crate::agent::domain::CompletionSummaryUpdate::Unchanged,
        }
    }

    fn member_of(hub_url: &str) -> PeersSettings {
        PeersSettings {
            role: PeerRole::Member,
            members: Vec::new(),
            hub: Some(PeerHub {
                url: hub_url.into(),
                secret: "s3cret".into(),
            }),
        }
    }

    // Each test names its Mac differently: the log of recorded names is shared by every
    // test in the process.

    #[test]
    fn hub_records_its_own_notification_before_sending_it() {
        crate::settings::init_settings_for_test();
        let sent = Mutex::new(Vec::new());

        let result = send_notification(
            &turn(),
            "studio-notify-hub",
            &PeersSettings::default(),
            "Tests pass. Deploy?",
            |text| {
                let recorded = recorded_notifications().contains(&"studio-notify-hub".to_string());
                sent.lock().unwrap().push((text.to_string(), recorded));
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(
            *sent.lock().unwrap(),
            [
                (
                    "[studio-notify-hub/alir-app main:0.1] Tests pass. (alir)".to_string(),
                    true
                ),
                ("Deploy?".to_string(), true),
            ]
        );
        let times = recorded_notifications()
            .iter()
            .filter(|name| *name == "studio-notify-hub")
            .count();
        assert_eq!(times, 1, "one record for the notification and its question");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_reports_to_the_hub_before_sending_and_records_nothing_itself() {
        crate::settings::init_settings_for_test();
        let hub = start_peer_server(PeerEndpoint::Hub {
            members: vec![PeerMember {
                name: "laptop-notify-member".into(),
                url: "http://127.0.0.1:1".into(),
                secret: "s3cret".into(),
            }],
        })
        .await;
        let peers = member_of(&hub.url);

        // On a blocking thread, as the event handler sends notifications.
        let reported_before_send = tokio::task::spawn_blocking(move || {
            let reported = Mutex::new(Vec::new());
            let result =
                send_notification(&turn(), "laptop-notify-member", &peers, "Done.", |_| {
                    reported.lock().unwrap().push(
                        recorded_notifications()
                            .iter()
                            .filter(|name| *name == "laptop-notify-member")
                            .count(),
                    );
                    Ok(())
                });
            assert!(result.is_ok());
            reported.into_inner().unwrap()
        })
        .await
        .unwrap();

        // One record, made by the hub's handler, already there when the member sent.
        assert_eq!(reported_before_send, [1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_sends_its_notification_when_the_hub_cannot_be_reached() {
        crate::settings::init_settings_for_test();
        let peers = member_of(&closed_port_url());

        let sent = tokio::task::spawn_blocking(move || {
            let sent = Mutex::new(Vec::new());
            let result =
                send_notification(&turn(), "laptop-notify-offline", &peers, "Done.", |text| {
                    sent.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            assert!(result.is_ok());
            sent.into_inner().unwrap()
        })
        .await
        .unwrap();

        assert_eq!(
            sent,
            ["[laptop-notify-offline/alir-app main:0.1] Done. (alir)"]
        );
        assert!(!recorded_notifications().contains(&"laptop-notify-offline".to_string()));
    }

    #[test]
    fn failed_send_is_reported_to_the_caller() {
        crate::settings::init_settings_for_test();

        let result = send_notification(
            &turn(),
            "studio-notify-fail",
            &PeersSettings::default(),
            "Done.",
            |_| Err("telegram down".to_string()),
        );

        assert_eq!(result.err(), Some("telegram down".to_string()));
    }

    #[test]
    fn agent_message_is_sent_marked_once() {
        let text = crate::inbound::ownership::agent_message_text(
            "PasM4MBP",
            "pods:0.1",
            "Deploy now? See https://example.com/a?b=1",
        );

        assert_eq!(
            send_message_body(7, &text)["text"],
            "🤖 [PasM4MBP/pods:0.1] Deploy now? See https://example.com/a?b=1"
        );
    }

    #[test]
    fn hub_records_itself_before_sending_an_agent_message() {
        crate::settings::init_settings_for_test();
        let mut seen = None;

        let result = send_agent_message(
            &PeersSettings::default(),
            "studio-agent-message-hub",
            "[studio-agent-message-hub/pods:0.1] Pod 3 is blocked",
            |text| {
                let recorded =
                    recorded_notifications().contains(&"studio-agent-message-hub".to_string());
                seen = Some((text.to_string(), recorded));
                Ok(())
            },
        );

        assert_eq!(result, Ok(()));
        assert_eq!(
            seen,
            Some((
                "[studio-agent-message-hub/pods:0.1] Pod 3 is blocked".to_string(),
                true
            ))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_reports_to_the_hub_before_sending_an_agent_message() {
        crate::settings::init_settings_for_test();
        let hub = start_peer_server(PeerEndpoint::Hub {
            members: vec![PeerMember {
                name: "laptop-agent-message-member".into(),
                url: "http://127.0.0.1:1".into(),
                secret: "s3cret".into(),
            }],
        })
        .await;
        let peers = member_of(&hub.url);

        // On a blocking thread, as the event handler sends.
        let reported_before_send = tokio::task::spawn_blocking(move || {
            let mut reported = None;
            let result = send_agent_message(&peers, "laptop-agent-message-member", "text", |_| {
                reported = Some(
                    recorded_notifications()
                        .iter()
                        .filter(|name| *name == "laptop-agent-message-member")
                        .count(),
                );
                Ok(())
            });
            assert_eq!(result, Ok(()));
            reported
        })
        .await
        .unwrap();

        assert_eq!(reported_before_send, Some(1));
    }

    #[test]
    fn failed_agent_message_send_is_reported_to_the_caller() {
        crate::settings::init_settings_for_test();

        let result = send_agent_message(
            &PeersSettings::default(),
            "studio-agent-message-fail",
            "text",
            |_| Err("telegram down".to_string()),
        );

        assert_eq!(result, Err("telegram down".to_string()));
    }

    use crate::peers::settings_with;

    #[tokio::test(flavor = "multi_thread")]
    async fn notify_reports_to_the_hub_the_settings_name() {
        crate::settings::init_settings_for_test();
        let hub = start_peer_server(PeerEndpoint::Hub {
            members: vec![laptop_at(
                "Laptop-Notify-With",
                "http://127.0.0.1:1",
                "s3cret",
            )],
        })
        .await;
        let settings = settings_with(&format!(
            r#"
            machine_name = "laptop-notify-with"
            machines = ["studio", "laptop-notify-with"]
            [peers]
            role = "member"
            [peers.hub]
            url = "{}"
            secret = "s3cret"
            "#,
            hub.url
        ));

        let sent = tokio::task::spawn_blocking(move || {
            let sent = Mutex::new(Vec::new());
            let result = notify_with(&settings, &turn(), "Done.", |text| {
                sent.lock().unwrap().push(text.to_string());
                Ok(())
            });
            assert!(result.is_ok());
            sent.into_inner().unwrap()
        })
        .await
        .unwrap();

        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with("[laptop-notify-with/"), "{sent:?}");
        // The hub records the member under the spelling it configured. A member that
        // recorded itself would have used its own.
        let recorded = |name: &str| {
            recorded_notifications()
                .iter()
                .filter(|recorded| *recorded == name)
                .count()
        };
        assert_eq!(
            recorded("Laptop-Notify-With"),
            1,
            "the hub recorded the report"
        );
        assert_eq!(
            recorded("laptop-notify-with"),
            0,
            "the member recorded nothing"
        );
    }

    #[test]
    fn notify_on_a_hub_records_the_settings_name_itself() {
        crate::settings::init_settings_for_test();
        let settings = settings_with(r#"machine_name = "studio-notify-with""#);

        let result = notify_with(&settings, &turn(), "Done.", |_| Ok(()));

        assert!(result.is_ok());
        let times = recorded_notifications()
            .iter()
            .filter(|name| *name == "studio-notify-with")
            .count();
        assert_eq!(times, 1);
    }

    #[test]
    fn live_hub_answers_from_the_settings_it_is_given() {
        let settings = settings_with(
            r#"
            machine_name = "studio"
            machines = ["studio", "laptop"]
            [[peers.members]]
            name = "laptop"
            url = "http://laptop.tailnet:50060"
            secret = "s3cret"
            "#,
        );
        let hub = LiveHub {
            settings: &settings,
            latest_notifier: || Some("laptop".into()),
            send: |_| Ok(()),
        };

        assert_eq!(hub.machine(), "studio");
        assert_eq!(hub.known_machines(), ["studio", "laptop"]);
        assert_eq!(hub.latest_notifier().as_deref(), Some("laptop"));
        assert_eq!(
            hub.members(),
            [laptop_at("laptop", "http://laptop.tailnet:50060", "s3cret")]
        );
    }

    #[test]
    fn live_hub_reads_the_latest_notifier_from_the_process_record() {
        let settings = settings_with(r#"machine_name = "studio""#);

        let hub = LiveHub::new(&settings);

        assert!(std::ptr::fn_addr_eq(
            hub.latest_notifier,
            crate::inbound::ownership::latest_notifier as fn() -> Option<String>
        ));
        assert!(std::ptr::fn_addr_eq(
            hub.send,
            crate::channels::send as fn(&str) -> Result<(), String>
        ));
    }

    #[tokio::test]
    async fn live_hub_tells_the_user_through_its_sender() {
        static TOLD: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let settings = settings_with(r#"machine_name = "studio""#);
        let hub = LiveHub {
            settings: &settings,
            latest_notifier: || None,
            send: |text| {
                TOLD.lock().unwrap().push(text.to_string());
                Ok(())
            },
        };

        hub.tell_user("Laptop is not reachable".into()).await;

        assert_eq!(*TOLD.lock().unwrap(), ["Laptop is not reachable"]);
    }

    #[tokio::test]
    async fn telling_the_user_sends_the_text() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&sent);

        tell_user_with("Laptop is not reachable".into(), move |text| {
            record.lock().unwrap().push(text.to_string());
            Ok(())
        })
        .await;

        assert_eq!(*sent.lock().unwrap(), ["Laptop is not reachable"]);
    }

    #[tokio::test]
    async fn a_failed_or_panicking_send_to_the_user_is_only_logged() {
        tell_user_with("hi".into(), |_| Err("telegram down".to_string())).await;
        tell_user_with("hi".into(), |_| panic!("send blew up")).await;
    }

    #[test]
    fn listener_reads_the_pause_switch_of_the_projector() {
        assert!(std::ptr::fn_addr_eq(
            pause_check(),
            crate::outbound::is_messaging_paused as fn() -> bool
        ));
    }

    use std::sync::Arc;

    use crate::store::HaroldStore;
    use crate::tests::inbound_messages;

    /// A hub named `studio` whose `machines` are `studio`, `laptop` and `mini`.
    struct FakeHub {
        latest_notifier: Option<String>,
        members: Vec<PeerMember>,
        told: Mutex<Vec<String>>,
    }

    impl FakeHub {
        fn new(latest_notifier: Option<&str>, members: Vec<PeerMember>) -> Self {
            Self {
                latest_notifier: latest_notifier.map(String::from),
                members,
                told: Mutex::new(Vec::new()),
            }
        }

        fn told(&self) -> Vec<String> {
            self.told.lock().unwrap().clone()
        }
    }

    impl HubEnv for FakeHub {
        fn machine(&self) -> &str {
            "studio"
        }

        fn known_machines(&self) -> Vec<String> {
            vec!["studio".into(), "laptop".into(), "mini".into()]
        }

        fn latest_notifier(&self) -> Option<String> {
            self.latest_notifier.clone()
        }

        fn members(&self) -> &[PeerMember] {
            &self.members
        }

        async fn tell_user(&self, text: String) {
            self.told.lock().unwrap().push(text);
        }
    }

    struct TestStore {
        store: Arc<HaroldStore>,
        path: std::path::PathBuf,
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    async fn hub_store() -> TestStore {
        let path = std::env::temp_dir().join(format!("harold-hub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let store = Arc::new(HaroldStore::open(&path).await.unwrap());
        TestStore { store, path }
    }

    fn update(update_id: i64, text: &str) -> Update {
        Update {
            update_id,
            message: message(7, text),
        }
    }

    fn laptop_at(name: &str, url: &str, secret: &str) -> PeerMember {
        PeerMember {
            name: name.into(),
            url: url.into(),
            secret: secret.into(),
        }
    }

    fn a_member() -> PeerEndpoint {
        PeerEndpoint::Member {
            hub: PeerHub {
                url: "http://127.0.0.1:1".into(),
                secret: "s3cret".into(),
            },
        }
    }

    #[test]
    fn reply_is_placed_by_its_tag_and_then_by_the_latest_notifier() {
        let after_laptop = FakeHub::new(Some("laptop"), Vec::new());
        let after_studio = FakeHub::new(Some("studio"), Vec::new());
        let before_any = FakeHub::new(None, Vec::new());
        let member = |name: &str| ReplyPlace::Member(name.to_string());

        assert_eq!(
            place_reply("yes, merge it", &after_laptop),
            member("laptop")
        );
        assert_eq!(
            place_reply("yes, merge it", &after_studio),
            ReplyPlace::Here
        );
        assert_eq!(place_reply("yes, merge it", &before_any), ReplyPlace::Here);
        assert_eq!(
            place_reply("[alir-app] go", &after_laptop),
            member("laptop")
        );
        // A tag that names a Mac settles it, whoever notified last.
        assert_eq!(
            place_reply("[studio/alir] go", &after_laptop),
            ReplyPlace::Here
        );
        assert_eq!(place_reply("[studio] go", &after_laptop), ReplyPlace::Here);
        assert_eq!(
            place_reply("[laptop/harold] go", &after_studio),
            member("laptop")
        );
        assert_eq!(place_reply("[MINI] go", &after_studio), member("mini"));
    }

    #[tokio::test]
    async fn reply_the_hub_owns_is_stored_as_already_decided() {
        let hub = hub_store().await;
        let env = FakeHub::new(Some("studio"), Vec::new());

        let offset = handle_updates(
            &hub.store,
            &env,
            vec![update(41, " yes, merge it ")],
            0,
            7,
            || false,
        )
        .await;

        assert_eq!(offset, 42);
        let stored = inbound_messages(&hub.store).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "yes, merge it");
        // forwarded = true: a member's report that lands before the event handler runs
        // must not make the handler drop a reply the hub already decided to keep.
        assert!(stored[0].forwarded);
        assert!(env.told().is_empty());
    }

    #[tokio::test]
    async fn untagged_reply_after_a_member_notified_is_forwarded_and_not_kept() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("laptop"),
            vec![laptop_at("laptop", &laptop.url, "s3cret")],
        );

        handle_updates(
            &hub.store,
            &env,
            vec![update(1, "yes, merge it")],
            0,
            7,
            || false,
        )
        .await;

        let forwarded = inbound_messages(&laptop.store).await;
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].text, "yes, merge it");
        assert!(forwarded[0].forwarded);
        assert!(inbound_messages(&hub.store).await.is_empty());
        assert!(env.told().is_empty());
    }

    #[tokio::test]
    async fn reply_tagged_for_the_hub_stays_on_the_hub_after_a_member_notified() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("laptop"),
            vec![laptop_at("laptop", &laptop.url, "s3cret")],
        );

        handle_updates(
            &hub.store,
            &env,
            vec![update(1, "[studio/alir] go")],
            0,
            7,
            || false,
        )
        .await;

        let kept = inbound_messages(&hub.store).await;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "[studio/alir] go");
        assert!(inbound_messages(&laptop.store).await.is_empty());
    }

    #[tokio::test]
    async fn forwarded_reply_carries_the_text_as_typed() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("studio"),
            vec![laptop_at("laptop", &laptop.url, "s3cret")],
        );
        let text = "[laptop/harold main:0.0] revert that\nthen say “done” ✅";

        handle_updates(&hub.store, &env, vec![update(1, text)], 0, 7, || false).await;

        let forwarded = inbound_messages(&laptop.store).await;
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].text, text);
        assert!(inbound_messages(&hub.store).await.is_empty());
    }

    #[tokio::test]
    async fn reply_is_forwarded_to_a_member_whose_name_differs_in_case() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("LAPTOP"),
            vec![laptop_at("Laptop", &laptop.url, "s3cret")],
        );

        handle_updates(&hub.store, &env, vec![update(1, "go on")], 0, 7, || false).await;

        assert_eq!(inbound_messages(&laptop.store).await.len(), 1);
        assert!(env.told().is_empty());
    }

    #[tokio::test]
    async fn reply_for_an_unreachable_member_tells_the_user_and_is_not_kept() {
        let hub = hub_store().await;
        let env = FakeHub::new(
            Some("laptop"),
            vec![laptop_at("laptop", &closed_port_url(), "s3cret")],
        );

        handle_updates(
            &hub.store,
            &env,
            vec![update(1, "yes, merge it")],
            0,
            7,
            || false,
        )
        .await;

        assert_eq!(env.told(), ["[studio] could not reach laptop"]);
        assert!(inbound_messages(&hub.store).await.is_empty());
    }

    #[tokio::test]
    async fn reply_for_a_mac_that_is_not_a_member_tells_the_user() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("studio"),
            vec![laptop_at("laptop", &laptop.url, "s3cret")],
        );

        handle_updates(&hub.store, &env, vec![update(1, "[mini] go")], 0, 7, || {
            false
        })
        .await;

        assert_eq!(env.told(), ["[studio] could not reach mini"]);
        assert!(inbound_messages(&hub.store).await.is_empty());
        assert!(inbound_messages(&laptop.store).await.is_empty());
    }

    #[tokio::test]
    async fn reply_a_member_refuses_tells_the_user() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("laptop"),
            vec![laptop_at("laptop", &laptop.url, "not-the-secret")],
        );

        handle_updates(
            &hub.store,
            &env,
            vec![update(1, "yes, merge it")],
            0,
            7,
            || false,
        )
        .await;

        assert_eq!(env.told(), ["[studio] could not reach laptop"]);
        assert!(inbound_messages(&hub.store).await.is_empty());
        assert!(inbound_messages(&laptop.store).await.is_empty());
    }

    #[tokio::test]
    async fn paused_hub_moves_past_updates_without_keeping_or_forwarding_them() {
        let hub = hub_store().await;
        let laptop = start_peer_server(a_member()).await;
        let env = FakeHub::new(
            Some("laptop"),
            vec![laptop_at("laptop", &laptop.url, "s3cret")],
        );

        let offset = handle_updates(
            &hub.store,
            &env,
            vec![update(8, "yes, merge it"), update(9, "[studio] go")],
            0,
            7,
            || true,
        )
        .await;

        assert_eq!(offset, 10);
        assert!(inbound_messages(&hub.store).await.is_empty());
        assert!(inbound_messages(&laptop.store).await.is_empty());
        assert!(env.told().is_empty());
    }

    #[tokio::test]
    async fn hub_skips_harold_messages_and_other_chats_and_keeps_the_rest_in_order() {
        let hub = hub_store().await;
        let env = FakeHub::new(None, Vec::new());
        let other_chat = Update {
            update_id: 3,
            message: message(8, "not for this bot"),
        };

        let offset = handle_updates(
            &hub.store,
            &env,
            vec![
                update(1, "first"),
                update(2, "🤖 [studio] echo"),
                other_chat,
                update(4, "second"),
            ],
            0,
            7,
            || false,
        )
        .await;

        assert_eq!(offset, 5);
        let texts: Vec<String> = inbound_messages(&hub.store)
            .await
            .into_iter()
            .map(|message| message.text)
            .collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[tokio::test]
    async fn no_updates_leave_the_offset_where_it_was() {
        let hub = hub_store().await;
        let env = FakeHub::new(None, Vec::new());

        assert_eq!(
            handle_updates(&hub.store, &env, Vec::new(), 17, 7, || false).await,
            17
        );
    }
}
