use std::sync::Arc;
use std::time::Duration;

use events::{ActorType, EventStore, ExpectedVersion, NewEvent, RotationPolicy};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const STREAM_ID: &str = "harold.events";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub pane_id: String,
    pub pane_label: String,
    pub last_user_prompt: String,
    pub assistant_message: String,
    pub main_context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyReceived {
    pub text: String,
}

fn rotation_policy() -> RotationPolicy {
    RotationPolicy::TimeWindow {
        window: Duration::from_secs(24 * 3600),
        max_bytes: Some(64 * 1024 * 1024),
    }
}

pub async fn open_store(path: &str) -> events::Result<Arc<EventStore>> {
    let store = EventStore::open_partitioned(path, rotation_policy()).await?;
    Ok(Arc::new(store))
}

pub async fn append_turn_completed(
    store: &EventStore,
    event: &TurnCompleted,
) -> events::Result<()> {
    store
        .append(
            STREAM_ID,
            ExpectedVersion::Any,
            vec![NewEvent {
                r#type: "TurnCompleted".into(),
                payload: json!(event),
                request_id: None,
                actor_id: "system:harold".into(),
                actor_type: ActorType::System,
            }],
        )
        .await?;
    Ok(())
}

pub async fn append_reply_received(
    store: &EventStore,
    event: &ReplyReceived,
) -> events::Result<()> {
    store
        .append(
            STREAM_ID,
            ExpectedVersion::Any,
            vec![NewEvent {
                r#type: "ReplyReceived".into(),
                payload: json!(event),
                request_id: None,
                actor_id: "system:harold".into(),
                actor_type: ActorType::System,
            }],
        )
        .await?;
    Ok(())
}
