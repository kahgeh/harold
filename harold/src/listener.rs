use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use events::EventStore;
use tokio::sync::watch;
use tracing::{Instrument, info, info_span};

use crate::settings::get_settings;
use crate::store::{ReplyReceived, append_reply_received};

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
    let out = match Command::new("sqlite3").arg(db_path()).arg(sql).output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (rowid_s, text) = line.split_once('|')?;
            let rowid = rowid_s.trim().parse::<i64>().ok()?;
            let text = text.trim().to_string();
            if text.is_empty() || text.starts_with('🤖') {
                return None;
            }
            Some((rowid, text))
        })
        .collect()
}

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

async fn poll(store: &EventStore) {
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
            match append_reply_received(store, &ReplyReceived { text }).await {
                Ok(()) => last_inbound_rowid().store(rowid, Ordering::Relaxed),
                Err(e) => tracing::warn!(error = %e, "failed to append ReplyReceived event"),
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
            match append_reply_received(store, &ReplyReceived { text }).await {
                Ok(()) => last_self_rowid().store(rowid, Ordering::Relaxed),
                Err(e) => tracing::warn!(error = %e, "failed to append ReplyReceived event"),
            }
        }
        .instrument(span)
        .await;
    }
}

pub async fn listen(store: Arc<EventStore>, mut shutdown: watch::Receiver<()>) {
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

    loop {
        tokio::select! {
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
