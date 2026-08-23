use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};
use tracing::{Instrument, info, info_span, warn};

use super::{AwayNotification, split_body, summarise_for_notification};
use crate::inbound::AgentAddress;
use crate::settings::get_settings;
use crate::store::{HaroldStore, InboundMessage, TurnCompleted, append_inbound_message};
use crate::util::sanitise_for_applescript;

// ===========================================================================
// Sending
// ===========================================================================

/// Low-level iMessage send — delivers `text` as-is (no prefix) to `recipient`.
pub(crate) fn send_imessage_to(text: &str, recipient: &str) -> Result<(), String> {
    let safe_text = sanitise_for_applescript(text);
    let safe_recipient = sanitise_for_applescript(recipient);
    let escaped = safe_text.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_recipient = safe_recipient.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"Messages\" to send \"{escaped}\" to buddy \"{escaped_recipient}\""
    );
    let status = Command::new("osascript")
        .args(["-e", &script])
        .status()
        .map_err(|error| format!("failed to start osascript: {error}"))?;
    if !status.success() {
        return Err(format!("osascript exited unsuccessfully: {status}"));
    }
    Ok(())
}

/// Send an iMessage notification with robot-emoji prefix.
fn send_raw_imessage(text: &str, recipient: &str) -> Result<(), String> {
    info!(msg = %text, "sending iMessage notification");
    send_imessage_to(&format!("🤖 {text}"), recipient)
}

/// Send a plain iMessage (confirmation/error) to the configured recipient.
pub(crate) fn send_imessage(msg: &str) -> Result<(), String> {
    info!(msg, "sending iMessage");
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        return Err("iMessage recipient is not configured".into());
    };
    send_imessage_to(msg, recipient)
}

fn recent_outgoing_texts(handle_id: i64) -> Vec<String> {
    let db_path = get_settings().chat_db.resolved_path();
    let out = Command::new("sqlite3")
        .arg("-json")
        .arg(db_path)
        .arg(format!(
            "SELECT text FROM message WHERE handle_id = {handle_id} AND is_from_me = 1 \
             ORDER BY ROWID DESC LIMIT 2;"
        ))
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.get("text")?.as_str().map(ToString::to_string))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum NotificationPlan<'a> {
    SendAll,
    SendQuestionOnly(&'a str),
    Skip,
}

fn normalize_notification_text(text: &str) -> &str {
    text.trim().trim_start_matches('🤖').trim()
}

fn notification_plan<'a>(
    recent_outgoing: &[&str],
    message: &str,
    question: Option<&'a str>,
) -> NotificationPlan<'a> {
    let Some(last) = recent_outgoing.first().copied() else {
        return NotificationPlan::SendAll;
    };
    let last = normalize_notification_text(last);

    if last == message.trim() {
        return question.map_or(NotificationPlan::Skip, NotificationPlan::SendQuestionOnly);
    }
    if question.is_some_and(|question| last == question.trim())
        && recent_outgoing
            .get(1)
            .is_some_and(|previous| normalize_notification_text(previous) == message.trim())
    {
        return NotificationPlan::Skip;
    }
    NotificationPlan::SendAll
}

// ---------------------------------------------------------------------------
// Away notification via iMessage — returns the source agent address
// ---------------------------------------------------------------------------

pub(crate) fn notify_away(
    turn: &TurnCompleted,
    _trace_id: &str,
) -> Result<AwayNotification, String> {
    let cfg = get_settings();
    let Some(recipient) = cfg.imessage.recipient.as_deref() else {
        warn!("iMessage recipient not configured");
        return Err("iMessage recipient is not configured".into());
    };
    let body = summarise_for_notification(&turn.assistant_message, &turn.last_user_prompt);

    let (main_body, question) = split_body(&body);
    let message = format!(
        "[{}] {} ({})",
        turn.pane_label,
        main_body.trim(),
        turn.main_context
    );

    let recent_outgoing = cfg
        .imessage
        .handle_ids
        .first()
        .map_or_else(Vec::new, |&id| recent_outgoing_texts(id));
    let recent_outgoing = recent_outgoing
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    match notification_plan(&recent_outgoing, &message, question) {
        NotificationPlan::SendAll => {
            send_raw_imessage(&message, recipient)?;
            info!("iMessage notification sent");
            if let Some(question) = question {
                send_raw_imessage(question, recipient)?;
                info!("iMessage question sent");
            }
        }
        NotificationPlan::SendQuestionOnly(question) => {
            send_raw_imessage(question, recipient)?;
            info!("iMessage question retry sent");
        }
        NotificationPlan::Skip => {
            info!("iMessage skipped (duplicate)");
            return Ok(AwayNotification::Skipped);
        }
    }

    Ok(AwayNotification::Sent(AgentAddress::TmuxPane {
        pane_id: turn.pane_id.clone(),
        label: turn.pane_label.clone(),
    }))
}

// ===========================================================================
// Listening
// ===========================================================================

static LAST_INBOUND_ROWID: OnceLock<AtomicI64> = OnceLock::new();
static LAST_SELF_ROWID: OnceLock<AtomicI64> = OnceLock::new();

fn last_inbound_rowid() -> &'static AtomicI64 {
    LAST_INBOUND_ROWID.get().expect("listener not initialised")
}

fn last_self_rowid() -> &'static AtomicI64 {
    LAST_SELF_ROWID.get().expect("listener not initialised")
}

fn db_path() -> String {
    get_settings().chat_db.resolved_path()
}

fn handle_ids() -> HashSet<i64> {
    get_settings().imessage.handle_ids.iter().copied().collect()
}

fn get_max_rowid() -> i64 {
    let out = Command::new("sqlite3")
        .arg(db_path())
        .arg("SELECT MAX(ROWID) FROM message;")
        .output()
        .ok();
    out.and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<i64>()
            .ok()
    })
    .unwrap_or(0)
}

fn query_messages(sql: &str) -> Vec<(i64, String)> {
    let out = match Command::new("sqlite3")
        .arg("-json")
        .arg(db_path())
        .arg(sql)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return vec![];
    };
    rows.into_iter()
        .filter_map(|row| {
            let rowid = row.get("ROWID")?.as_i64()?;
            let text = row.get("text")?.as_str()?.trim().to_string();
            if text.is_empty() || text.starts_with('🤖') {
                return None;
            }
            Some((rowid, text))
        })
        .collect()
}

/// All interpolated values are typed integers from trusted config/internal state — never user input.
fn fetch_messages(last_rowid: i64, is_from_me: u8) -> Vec<(i64, String)> {
    let ids = handle_ids();
    if ids.is_empty() {
        return vec![];
    }
    let id_list = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    query_messages(&format!(
        "SELECT ROWID, text FROM message \
         WHERE ROWID > {last_rowid} AND handle_id IN ({id_list}) AND is_from_me = {is_from_me} \
           AND text IS NOT NULL AND length(text) > 0 \
         ORDER BY ROWID ASC;"
    ))
}

fn fetch_inbound(last_rowid: i64) -> Vec<(i64, String)> {
    fetch_messages(last_rowid, 0)
}

fn fetch_self(last_rowid: i64) -> Vec<(i64, String)> {
    fetch_messages(last_rowid, 1)
}

async fn poll(store: &HaroldStore) {
    let inbound_rowid = last_inbound_rowid().load(Ordering::Relaxed);
    let self_rowid = last_self_rowid().load(Ordering::Relaxed);

    let (inbound, self_msgs) =
        tokio::task::spawn_blocking(move || (fetch_inbound(inbound_rowid), fetch_self(self_rowid)))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "fetch task panicked");
                (vec![], vec![])
            });

    for (rowid, text) in inbound {
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("listener_inbound", trace_id = %trace_id, rowid = rowid);

        async {
            info!("iMessage received (inbound)");
            match append_inbound_message(store, &InboundMessage { text }).await {
                Ok(()) => last_inbound_rowid().store(rowid, Ordering::Relaxed),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to append InboundMessageReceived event")
                }
            }
        }
        .instrument(span)
        .await;
    }

    for (rowid, text) in self_msgs {
        let trace_id = uuid::Uuid::new_v4().to_string();
        let span = info_span!("listener_self", trace_id = %trace_id, rowid = rowid);

        async {
            info!("iMessage received (self)");
            match append_inbound_message(store, &InboundMessage { text }).await {
                Ok(()) => last_self_rowid().store(rowid, Ordering::Relaxed),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to append InboundMessageReceived event")
                }
            }
        }
        .instrument(span)
        .await;
    }
}

fn start_watcher(chat_db_path: &str) -> Option<(RecommendedWatcher, mpsc::UnboundedReceiver<()>)> {
    let parent = Path::new(chat_db_path).parent()?;
    let db_name = Path::new(chat_db_path).file_name()?.to_str()?.to_string();
    let wal_name = format!("{db_name}-wal");

    let (tx, rx) = mpsc::unbounded_channel();

    let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "fs watcher event error");
                return;
            }
        };
        if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
            return;
        }
        let targets_chat_db = event.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == db_name || n == wal_name)
        });
        if targets_chat_db {
            let _ = tx.send(());
        }
    });

    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "failed to create fs watcher, falling back to poll-only");
            return None;
        }
    };

    if let Err(e) = watcher.watch(parent, RecursiveMode::NonRecursive) {
        warn!(error = %e, "failed to watch chat.db directory, falling back to poll-only");
        return None;
    }

    info!(path = %parent.display(), "fs watcher active on chat.db directory");
    Some((watcher, rx))
}

pub(crate) async fn listen(store: Arc<HaroldStore>, mut shutdown: watch::Receiver<()>) {
    let initial = tokio::task::spawn_blocking(get_max_rowid)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "get_max_rowid task panicked, starting from 0");
            0
        });
    LAST_INBOUND_ROWID
        .set(AtomicI64::new(initial))
        .expect("listen called more than once");
    LAST_SELF_ROWID
        .set(AtomicI64::new(initial))
        .expect("listen called more than once");
    info!(initial_rowid = initial, "iMessage listener started");

    // Keep _watcher alive (dropping it stops watching). In the fallback path,
    // _keep_tx stays alive so fs_rx.recv() pends forever rather than returning None.
    let (_watcher, mut fs_rx, _keep_tx) = match start_watcher(&db_path()) {
        Some((watcher, rx)) => (Some(watcher), rx, None),
        None => {
            let (tx, rx) = mpsc::unbounded_channel();
            (None, rx, Some(tx))
        }
    };

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                info!("iMessage listener shutting down");
                break;
            }
            _ = fs_rx.recv() => {
                while fs_rx.try_recv().is_ok() {}
                poll(&store).await;
            }
            // The 5s timer restarts after every fs-triggered poll, which is intentional:
            // if fs events are flowing, we don't need the fallback.
            () = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                poll(&store).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "imessage_tests.rs"]
mod tests;
