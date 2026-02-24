use std::sync::Arc;
use std::time::Duration;

use events::{EventStore, ExpectedVersion, NewEvent, RotationPolicy};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const STREAM_TURNS: &str = "harold.turns";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub pane_id: String,
    pub pane_label: String,
    pub last_user_prompt: String,
    pub assistant_message: String,
    pub main_context: String,
}

pub async fn open_store(path: &str) -> events::Result<Arc<EventStore>> {
    let store = EventStore::open_partitioned(
        path,
        RotationPolicy::TimeWindow {
            window: Duration::from_secs(24 * 3600),
            max_bytes: Some(64 * 1024 * 1024),
        },
    )
    .await?;
    Ok(Arc::new(store))
}

pub async fn append_turn_completed(
    store: &EventStore,
    event: &TurnCompleted,
) -> events::Result<()> {
    store
        .append(
            STREAM_TURNS,
            ExpectedVersion::Any,
            vec![NewEvent {
                r#type: "TurnCompleted".into(),
                payload: json!(event),
            }],
        )
        .await?;
    Ok(())
}
