use std::path::{Path, PathBuf};

use events::EventStreamVersion;

use super::{
    HaroldStore, InboundMessage, TurnCompleted, append_inbound_message, append_turn_completed,
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn appended_turn_completed_is_readable_from_refreshed_stream() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let turn = TurnCompleted {
        pane_id: "%7".into(),
        pane_label: "harold:0.1".into(),
        last_user_prompt: "Update the event store".into(),
        assistant_message: "The event store was updated.".into(),
        main_context: "harold".into(),
    };

    append_turn_completed(&store, &turn).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].r#type, "TurnCompleted");
    let stored: TurnCompleted = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(stored.pane_id, "%7");
    assert_eq!(stored.pane_label, "harold:0.1");
    assert_eq!(stored.last_user_prompt, "Update the event store");
    assert_eq!(stored.assistant_message, "The event store was updated.");
    assert_eq!(stored.main_context, "harold");
}

#[tokio::test]
async fn staging_is_ordered_checkpointed_and_idempotent() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_turn_completed(
        &store,
        &TurnCompleted {
            pane_id: "%1".into(),
            pane_label: "harold:0.1".into(),
            last_user_prompt: "first".into(),
            assistant_message: "done".into(),
            main_context: "harold".into(),
        },
    )
    .await
    .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "second".into(),
        },
    )
    .await
    .unwrap();

    assert_eq!(store.stage_unhandled_events(500).await.unwrap(), 2);
    assert_eq!(store.last_processed_version().await.unwrap().get(), 2);

    let first = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(first.event_version.get(), 1);
    assert_eq!(first.event_type, "TurnCompleted");
    store.mark_delivered(&first.event_id).await.unwrap();

    let second = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(second.event_version.get(), 2);
    assert_eq!(second.event_type, "InboundMessageReceived");
    store.mark_delivered(&second.event_id).await.unwrap();

    assert!(store.next_pending_delivery().await.unwrap().is_none());
    assert_eq!(store.stage_unhandled_events(500).await.unwrap(), 0);
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn failed_delivery_remains_pending_until_marked_delivered() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "retry me".into(),
        },
    )
    .await
    .unwrap();
    store.stage_unhandled_events(500).await.unwrap();
    let pending = store.next_pending_delivery().await.unwrap().unwrap();

    store
        .record_delivery_failure(&pending.event_id, "temporary failure")
        .await
        .unwrap();

    let retry = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(retry.event_id, pending.event_id);
    store.mark_delivered(&retry.event_id).await.unwrap();
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn state_migrations_are_idempotent_and_reject_changed_checksums() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    drop(store);
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let conn = store.state.connect().unwrap();
    conn.execute(
        "UPDATE _migrations SET checksum = 'changed' WHERE name = '001_last_processed_event'",
        (),
    )
    .await
    .unwrap();
    drop(conn);
    drop(store);

    let error = HaroldStore::open(directory.path())
        .await
        .err()
        .expect("changed migration checksum should fail");
    assert!(error.to_string().contains("checksum changed"));
}

#[tokio::test]
async fn reopening_preserves_checkpoint_and_pending_delivery_without_duplication() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "survive restart".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(store.stage_unhandled_events(500).await.unwrap(), 1);
    let pending_id = store
        .next_pending_delivery()
        .await
        .unwrap()
        .unwrap()
        .event_id;
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    assert_eq!(reopened.last_processed_version().await.unwrap().get(), 1);
    assert_eq!(reopened.stage_unhandled_events(500).await.unwrap(), 0);
    let pending = reopened.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(pending.event_id, pending_id);
    reopened.mark_delivered(&pending.event_id).await.unwrap();
    assert!(reopened.next_pending_delivery().await.unwrap().is_none());
}
