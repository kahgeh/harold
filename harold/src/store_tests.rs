use std::path::{Path, PathBuf};

use events::EventStreamVersion;

use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentMonitorHealthChanged,
    AgentPaneDeparted, AgentPaneObservation, AgentPaneObserved, AgentScreenObserved,
    ObservedAgentState, WorkSummaryUpdate,
};

use super::{
    HaroldStore, InboundMessage, TurnCompleted, append_agent_events, append_inbound_message,
    append_turn_completed,
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

fn agent_incarnation() -> AgentIncarnation {
    AgentIncarnation {
        pane_id: "%7".into(),
        pane_pid: 10,
        agent_pid: 20,
        agent_started_at_ms: 1_000,
        provider_id: "codex".into(),
    }
}

fn agent_pane() -> AgentPaneObservation {
    AgentPaneObservation {
        incarnation: agent_incarnation(),
        tmux_target: "harold:2.1".into(),
        session_name: "harold".into(),
        window_index: 2,
        pane_index: 1,
        working_directory: "/work/harold".into(),
        provider_display_name: "Codex".into(),
        observed_at_ms: 100,
    }
}

#[tokio::test]
async fn agent_events_round_trip_with_stable_types_and_normalized_payloads() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let events = vec![
        AgentEvent::PaneObserved(AgentPaneObserved { pane: agent_pane() }),
        AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: agent_incarnation(),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Set("  Fix\u{1b}[31m tests\n".into()),
            observed_at_ms: 101,
        }),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: agent_incarnation(),
            state: Some(ObservedAgentState::Idle),
            classifier_id: "screen-v1".into(),
            fallback_summary: None,
            observed_at_ms: 102,
        }),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: agent_incarnation(),
            state: None,
            classifier_id: "screen-v1".into(),
            fallback_summary: Some("  Review\tprojector ".into()),
            observed_at_ms: 103,
        }),
        AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: agent_incarnation(),
            state: Some(ObservedAgentState::Busy),
            classifier_id: "screen-v2".into(),
            fallback_summary: Some("Ship release".into()),
            observed_at_ms: 104,
        }),
        AgentEvent::PaneDeparted(AgentPaneDeparted {
            incarnation: agent_incarnation(),
            observed_at_ms: 105,
        }),
        AgentEvent::MonitorHealthChanged(AgentMonitorHealthChanged {
            component: "screen".into(),
            healthy: false,
            reason_code: "capture_failed".into(),
            observed_at_ms: 106,
        }),
    ];

    let appended = append_agent_events(&store, events).await.unwrap();
    assert_eq!(appended.first_version.get(), 1);
    assert_eq!(appended.last_version.get(), 7);
    assert_eq!(
        appended
            .events
            .iter()
            .map(|event| event.version.get())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7]
    );

    let stored = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(stored[0].r#type, "AgentPaneObserved");
    let pane_observed: AgentPaneObserved =
        serde_json::from_value(stored[0].payload.clone()).unwrap();
    assert_eq!(pane_observed.pane, agent_pane());
    assert_eq!(stored[1].r#type, "AgentLifecycleObserved");
    let lifecycle: AgentLifecycleObserved =
        serde_json::from_value(stored[1].payload.clone()).unwrap();
    assert_eq!(
        lifecycle.work_summary,
        WorkSummaryUpdate::Set("Fix tests".into())
    );
    assert_eq!(stored[2].r#type, "AgentScreenObserved");
    let state_only: AgentScreenObserved =
        serde_json::from_value(stored[2].payload.clone()).unwrap();
    assert_eq!(state_only.state, Some(ObservedAgentState::Idle));
    assert_eq!(state_only.fallback_summary, None);
    let summary_only: AgentScreenObserved =
        serde_json::from_value(stored[3].payload.clone()).unwrap();
    assert_eq!(summary_only.state, None);
    assert_eq!(
        summary_only.fallback_summary.as_deref(),
        Some("Review projector")
    );
    let both: AgentScreenObserved = serde_json::from_value(stored[4].payload.clone()).unwrap();
    assert_eq!(both.state, Some(ObservedAgentState::Busy));
    assert_eq!(both.fallback_summary.as_deref(), Some("Ship release"));
    assert_eq!(stored[5].r#type, "AgentPaneDeparted");
    let departed: AgentPaneDeparted = serde_json::from_value(stored[5].payload.clone()).unwrap();
    assert_eq!(departed.incarnation, agent_incarnation());
    assert_eq!(departed.observed_at_ms, 105);
    assert_eq!(stored[6].r#type, "AgentMonitorHealthChanged");
    let health: AgentMonitorHealthChanged =
        serde_json::from_value(stored[6].payload.clone()).unwrap();
    assert_eq!(health.component, "screen");
    assert!(!health.healthy);
    assert_eq!(health.reason_code, "capture_failed");
}

#[tokio::test]
async fn pane_and_lifecycle_are_one_contiguous_agent_append_batch() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    let appended = append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved { pane: agent_pane() }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: agent_incarnation(),
                state: ObservedAgentState::Busy,
                adapter_id: "hook-v1".into(),
                work_summary: WorkSummaryUpdate::Unchanged,
                observed_at_ms: 101,
            }),
        ],
    )
    .await
    .unwrap();

    assert_eq!(appended.first_version.get(), 1);
    assert_eq!(appended.last_version.get(), 2);
    assert_eq!(
        appended
            .events
            .iter()
            .map(|event| event.r#type.as_str())
            .collect::<Vec<_>>(),
        vec!["AgentPaneObserved", "AgentLifecycleObserved"]
    );
}

#[tokio::test]
async fn all_absent_screen_observation_is_not_appended() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    let appended = append_agent_events(
        &store,
        vec![AgentEvent::ScreenObserved(AgentScreenObserved {
            incarnation: agent_incarnation(),
            state: None,
            classifier_id: "screen-v1".into(),
            fallback_summary: None,
            observed_at_ms: 100,
        })],
    )
    .await
    .unwrap();

    assert!(appended.events.is_empty());
    assert_eq!(store.stream().current_version().await.unwrap().get(), 0);
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
