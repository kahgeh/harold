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
    if let Some(id) = cfg.imessage.handle_id && id > 0 {
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
    if ids.is_empty() {
        return vec![];
    }
    // id_list is built from HashSet<i64> — all values are numeric, no injection risk.
    // last_rowid is i64 from AtomicI64 — numeric, safe to interpolate.
    let id_list = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT ROWID, text FROM message \
         WHERE ROWID > {last_rowid} AND handle_id IN ({id_list}) AND is_from_me = 0 \
         ORDER BY ROWID ASC;"
    );

    let out = match Command::new("sqlite3").arg(db_path()).arg(&sql).output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (rowid_s, text) = line.split_once('|')?;
            let rowid = rowid_s.trim().parse::<i64>().ok()?;
            let text = text.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some((rowid, text))
            }
        })
        .collect()
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
