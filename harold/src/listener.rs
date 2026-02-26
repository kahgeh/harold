use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use events::EventStore;
use tokio::sync::watch;
use tracing::info;

use crate::settings::get_settings;
use crate::store::{ReplyReceived, append_reply_received};

static LAST_PROCESSED_ROWID: OnceLock<AtomicI64> = OnceLock::new();

fn last_rowid() -> &'static AtomicI64 {
    LAST_PROCESSED_ROWID
        .get()
        .expect("listener not initialised")
}

fn db_path() -> String {
    get_settings().chat_db.resolved_path()
}

fn handle_ids() -> HashSet<i64> {
    let cfg = get_settings();
    let mut ids = HashSet::new();
    if let Some(id) = cfg.imessage.handle_id
        && id > 0
    {
        ids.insert(id);
    }
    if let Some(extras) = &cfg.imessage.extra_handle_ids {
        ids.extend(extras);
    }
    ids
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

fn fetch_new_messages(last_rowid: i64) -> Vec<(i64, String)> {
    let ids = handle_ids();
    let self_handle_id = get_settings().imessage.self_handle_id.filter(|&id| id > 0);

    if ids.is_empty() && self_handle_id.is_none() {
        return vec![];
    }

    // id_list and self_handle_id are i64 — numeric, no injection risk.
    // last_rowid is i64 from AtomicI64 — numeric, safe to interpolate.
    //
    // Messages sent from your phone appear in chat.db as is_from_me=1 on your own Apple ID
    // handle (self_handle_id). The standard inbound path is is_from_me=0 on the sender's handle.
    let sql = match (ids.is_empty(), self_handle_id) {
        (false, Some(self_id)) => {
            let id_list = ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "SELECT ROWID, text FROM message \
                 WHERE ROWID > {last_rowid} \
                   AND ((handle_id IN ({id_list}) AND is_from_me = 0) \
                     OR (handle_id = {self_id} AND is_from_me = 1)) \
                 ORDER BY ROWID ASC;"
            )
        }
        (false, None) => {
            let id_list = ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "SELECT ROWID, text FROM message \
                 WHERE ROWID > {last_rowid} AND handle_id IN ({id_list}) AND is_from_me = 0 \
                 ORDER BY ROWID ASC;"
            )
        }
        (true, Some(self_id)) => {
            format!(
                "SELECT ROWID, text FROM message \
                 WHERE ROWID > {last_rowid} AND handle_id = {self_id} AND is_from_me = 1 \
                 ORDER BY ROWID ASC;"
            )
        }
        (true, None) => return vec![],
    };

    let out = match Command::new("sqlite3").arg(db_path()).arg(&sql).output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let mut messages: Vec<(i64, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (rowid_s, text) = line.split_once('|')?;
            let rowid = rowid_s.trim().parse::<i64>().ok()?;
            let text = text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some((rowid, text))
        })
        .collect();

    // Deduplicate: iMessage sync can produce two rows for the same message —
    // one as is_from_me=1 on self_handle_id (phone sync) and one as is_from_me=0
    // on the sender's handle (normal inbound). Keep only the last occurrence of
    // each unique text so we process it once at the highest ROWID.
    messages.dedup_by(|a, b| {
        if a.1 == b.1 {
            // dedup_by: `a` is the later element (higher ROWID), `b` is the earlier one and
            // is retained. Copy the higher ROWID from `a` into `b` before `a` is dropped.
            b.0 = a.0;
            true
        } else {
            false
        }
    });

    messages
}

async fn poll(store: &EventStore) {
    let current_rowid = last_rowid().load(Ordering::Relaxed);
    let messages = tokio::task::spawn_blocking(move || fetch_new_messages(current_rowid))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "fetch_new_messages task panicked");
            vec![]
        });

    for (rowid, text) in messages {
        // Mark as seen before appending — prevents re-processing on crash.
        last_rowid().store(rowid, Ordering::Relaxed);
        info!(rowid, "iMessage received");

        if let Err(e) = append_reply_received(store, &ReplyReceived { text }).await {
            // rowid already advanced; this message is skipped rather than reprocessed.
            tracing::warn!(error = %e, "failed to append ReplyReceived event");
        }
    }
}

pub async fn listen(store: Arc<EventStore>, mut shutdown: watch::Receiver<()>) {
    let initial = tokio::task::spawn_blocking(get_max_rowid)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "get_max_rowid task panicked, starting from 0");
            0
        });
    LAST_PROCESSED_ROWID
        .set(AtomicI64::new(initial))
        .expect("listen called more than once");
    info!(initial_rowid = initial, "iMessage listener started");

    loop {
        tokio::select! {
            // Sender dropped signals shutdown; Err(RecvError) means channel closed.
            _ = shutdown.changed() => {
                info!("iMessage listener shutting down");
                break;
            }
            () = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                poll(&store).await;
            }
        }
    }
}
