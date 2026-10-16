use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::store::{EventVersion, NewEvent};
use serde_json::json;
use tokio::time::Instant;

use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentMonitorHealthChanged,
    AgentPaneDeparted, AgentPaneObservation, AgentPaneObserved, AgentScreenObserved,
    CompletionSummaryUpdate, EffectiveAgentState, ObservedAgentState, WorkSummaryUpdate,
};

use super::{
    AppMessage, AppPushSubscribed, AppPushUnsubscribed, AppReplyState, AppSender, HaroldStore,
    InboundMessage, ReceivedReply, TimedPromptHandled, TimedPromptOutcome, TurnCompleted,
    append_agent_events, append_app_push_subscribed, append_app_push_unsubscribed,
    append_app_reply, append_inbound_message, append_messaging_paused_changed,
    append_timed_prompt_handled, append_tts_mute_changed, append_turn_completed,
};
use super::{FailedWrite, StoreError, WriteFault};

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

fn assert_persisted_files_exclude(root: &Path, sentinel: &[u8]) {
    for entry in std::fs::read_dir(root).expect("read store directory") {
        let path = entry.expect("read store entry").path();
        if path.is_dir() {
            assert_persisted_files_exclude(&path, sentinel);
            continue;
        }
        let bytes = std::fs::read(&path).expect("read persisted store file");
        assert!(
            !bytes
                .windows(sentinel.len())
                .any(|window| window == sentinel),
            "sentinel leaked into persisted store file {}",
            path.display()
        );
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

fn assert_full_incarnation(incarnation: &AgentIncarnation) {
    assert_eq!(incarnation.pane_id, "%7");
    assert_eq!(incarnation.pane_pid, 10);
    assert_eq!(incarnation.agent_pid, 20);
    assert_eq!(incarnation.agent_started_at_ms, 1_000);
    assert_eq!(incarnation.provider_id, "codex");
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
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(stored[0].r#type, "AgentPaneObserved");
    let pane_observed: AgentPaneObserved =
        serde_json::from_value(stored[0].payload.clone()).unwrap();
    assert_full_incarnation(&pane_observed.pane.incarnation);
    assert_eq!(pane_observed.pane, agent_pane());
    assert_eq!(stored[1].r#type, "AgentLifecycleObserved");
    let lifecycle: AgentLifecycleObserved =
        serde_json::from_value(stored[1].payload.clone()).unwrap();
    assert_eq!(
        lifecycle.work_summary,
        WorkSummaryUpdate::Set("Fix tests".into())
    );
    assert_full_incarnation(&lifecycle.incarnation);
    assert_eq!(stored[2].r#type, "AgentScreenObserved");
    let state_only: AgentScreenObserved =
        serde_json::from_value(stored[2].payload.clone()).unwrap();
    assert_eq!(state_only.state, Some(ObservedAgentState::Idle));
    assert_eq!(state_only.fallback_summary, None);
    assert_full_incarnation(&state_only.incarnation);
    let summary_only: AgentScreenObserved =
        serde_json::from_value(stored[3].payload.clone()).unwrap();
    assert_eq!(summary_only.state, None);
    assert_eq!(
        summary_only.fallback_summary.as_deref(),
        Some("Review projector")
    );
    assert_full_incarnation(&summary_only.incarnation);
    let both: AgentScreenObserved = serde_json::from_value(stored[4].payload.clone()).unwrap();
    assert_eq!(both.state, Some(ObservedAgentState::Busy));
    assert_eq!(both.fallback_summary.as_deref(), Some("Ship release"));
    assert_full_incarnation(&both.incarnation);
    assert_eq!(stored[5].r#type, "AgentPaneDeparted");
    let departed: AgentPaneDeparted = serde_json::from_value(stored[5].payload.clone()).unwrap();
    assert_full_incarnation(&departed.incarnation);
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
    assert!(
        store
            .stream()
            .load_after_version(EventVersion::start(), 1)
            .await
            .unwrap()
            .is_empty()
    );
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
        agent_incarnation: None,
        work_summary: CompletionSummaryUpdate::Unchanged,
    };

    append_turn_completed(&store, &turn).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
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
            agent_incarnation: None,
            work_summary: CompletionSummaryUpdate::Unchanged,
        },
    )
    .await
    .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "second".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();

    let batch = store.project_unhandled_events(500).await.unwrap();
    assert_eq!(batch.applied, 2);
    assert_eq!(batch.through_event_version.get(), 2);
    assert!(!batch.snapshot_changed);
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
    assert_eq!(
        store.project_unhandled_events(500).await.unwrap().applied,
        0
    );
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
            forwarded: false,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(500).await.unwrap();
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
async fn a_pending_delivery_carries_its_failures_across_a_reopen() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "count me".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(500).await.unwrap();
    let pending = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(pending.attempt_count, 0);

    for _ in 0..2 {
        store
            .record_delivery_failure(&pending.event_id, "temporary failure")
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .next_pending_delivery()
            .await
            .unwrap()
            .unwrap()
            .attempt_count,
        2
    );
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    let retry = reopened.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(retry.event_id, pending.event_id);
    assert_eq!(retry.attempt_count, 2);
}

#[tokio::test]
async fn state_migrations_are_idempotent_and_reject_changed_checksums() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    drop(store);
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let conn = store.state.connect().unwrap();
    conn.execute(
        "UPDATE _migrations SET checksum = 'changed' WHERE name = '001_initial'",
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
            forwarded: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        store.project_unhandled_events(500).await.unwrap().applied,
        1
    );
    let pending_id = store
        .next_pending_delivery()
        .await
        .unwrap()
        .unwrap()
        .event_id;
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    assert_eq!(reopened.last_processed_version().await.unwrap().get(), 1);
    assert_eq!(
        reopened
            .project_unhandled_events(500)
            .await
            .unwrap()
            .applied,
        0
    );
    let pending = reopened.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(pending.event_id, pending_id);
    reopened.mark_delivered(&pending.event_id).await.unwrap();
    assert!(reopened.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn fresh_schema_has_one_initial_record_across_reopen() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    drop(store);
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let conn = store.state.connect().unwrap();
    let mut rows = conn
        .query("SELECT name FROM _migrations ORDER BY id", ())
        .await
        .unwrap();
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        names.push(row.get_value(0).unwrap().as_text().unwrap().to_string());
    }
    assert_eq!(names, ["001_initial", "002_web_app"]);
}

#[tokio::test]
async fn conflicting_schema_fails_without_partial_initialization_or_data_loss() {
    let directory = TestDirectory::new();
    let state_path = directory.path().join(super::STATE_DATABASE);
    let database = turso::Builder::new_local(state_path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    super::configure_state_database(&conn).await.unwrap();
    conn.execute_batch(
        "CREATE TABLE agent_panes (existing_data TEXT NOT NULL);
         INSERT INTO agent_panes VALUES ('preserve this record');",
    )
    .await
    .unwrap();

    assert!(HaroldStore::open(directory.path()).await.is_err());

    let mut rows = conn
        .query("SELECT existing_data FROM agent_panes", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next()
            .await
            .unwrap()
            .unwrap()
            .get_value(0)
            .unwrap()
            .as_text()
            .map(String::as_str),
        Some("preserve this record")
    );
    assert!(rows.next().await.unwrap().is_none());
    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
            (),
        )
        .await
        .unwrap();
    assert_eq!(
        rows.next()
            .await
            .unwrap()
            .unwrap()
            .get_value(0)
            .unwrap()
            .as_text()
            .map(String::as_str),
        Some("agent_panes")
    );
    assert!(rows.next().await.unwrap().is_none());
}

#[tokio::test]
async fn unknown_schema_record_is_rejected_without_modifying_pending_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "keep pending".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(500).await.unwrap();
    let pending_id = store
        .next_pending_delivery()
        .await
        .unwrap()
        .unwrap()
        .event_id;
    let conn = store.state.connect().unwrap();
    conn.execute(
        "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES ('unknown_schema', 'unknown', 0)",
        (),
    )
    .await
    .unwrap();
    // The store folder's lock lets one store open at a time.
    drop(conn);
    drop(store);

    let error = HaroldStore::open(directory.path())
        .await
        .err()
        .expect("unknown schema must fail");
    assert!(error.to_string().contains("incompatible state schema"));

    let state_path = directory.path().join(super::STATE_DATABASE);
    let database = turso::Builder::new_local(state_path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = database.connect().unwrap();
    assert_eq!(
        super::last_processed_version_from(&conn)
            .await
            .unwrap()
            .get(),
        1
    );
    let mut rows = conn
        .query(
            "SELECT event_id FROM delivery_outbox WHERE delivered_at_ms IS NULL",
            (),
        )
        .await
        .unwrap();
    let row = rows
        .next()
        .await
        .unwrap()
        .expect("the delivery is still pending");
    assert_eq!(
        row.get_value(0).unwrap().as_text().map(String::as_str),
        Some(pending_id.as_str())
    );
    assert!(rows.next().await.unwrap().is_none());
    let mut rows = conn
        .query("SELECT COUNT(*) FROM _migrations", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next()
            .await
            .unwrap()
            .unwrap()
            .get_value(0)
            .unwrap()
            .as_integer(),
        Some(&3)
    );
}

#[tokio::test]
async fn fresh_schema_enforces_generated_summary_constraints() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: agent_pane(),
        })],
    )
    .await
    .unwrap();
    store.project_unhandled_events(500).await.unwrap();
    let pane = store.load_agent_snapshot().await.unwrap().panes.remove(0);
    assert_eq!(pane.generated_work_summary, None);
    assert_eq!(pane.generated_summary_basis_version, None);

    let conn = store.state.connect().unwrap();
    for sql in [
        "UPDATE agent_panes SET summary_basis_version = -1",
        "UPDATE agent_panes SET generated_work_summary = 'Reviewing tests'",
        "UPDATE agent_panes SET generated_summary_basis_version = 1",
        "UPDATE agent_panes SET generated_work_summary = '', generated_summary_basis_version = 1",
        "UPDATE agent_panes SET generated_work_summary = 'Reviewing tests', generated_summary_basis_version = -1",
    ] {
        assert!(
            conn.execute(sql, ()).await.is_err(),
            "accepted invalid update: {sql}"
        );
    }
    assert!(conn.execute(
        "UPDATE agent_panes SET generated_work_summary = ?1, generated_summary_basis_version = 1",
        ("x".repeat(161),),
    ).await.is_err());
    conn.execute(
        "UPDATE agent_panes SET generated_work_summary = ?1, generated_summary_basis_version = 1",
        ("x".repeat(160),),
    )
    .await
    .unwrap();
    let updated = store.load_agent_snapshot().await.unwrap().panes.remove(0);
    assert_eq!(updated.generated_work_summary, Some("x".repeat(160)));
    assert_eq!(updated.generated_summary_basis_version.unwrap().get(), 1);
}

#[tokio::test]
async fn projection_stages_only_deliveries_and_projects_agent_events_in_one_batch() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: agent_pane(),
        })],
    )
    .await
    .unwrap();
    append_turn_completed(
        &store,
        &TurnCompleted {
            pane_id: "%7".into(),
            pane_label: "harold:2.1".into(),
            last_user_prompt: "project state".into(),
            assistant_message: "projected".into(),
            main_context: "harold".into(),
            agent_incarnation: None,
            work_summary: CompletionSummaryUpdate::Unchanged,
        },
    )
    .await
    .unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: agent_incarnation(),
            state: ObservedAgentState::Busy,
            adapter_id: "hook-v1".into(),
            work_summary: WorkSummaryUpdate::Set("Fix the projector".into()),
            observed_at_ms: 101,
        })],
    )
    .await
    .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "continue".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::MonitorHealthChanged(
            AgentMonitorHealthChanged {
                component: "screen".into(),
                healthy: false,
                reason_code: "capture_failed".into(),
                observed_at_ms: 102,
            },
        )],
    )
    .await
    .unwrap();

    let batch = store.project_unhandled_events(500).await.unwrap();
    assert_eq!(batch.applied, 5);
    assert_eq!(batch.through_event_version.get(), 5);
    assert!(batch.snapshot_changed);
    assert_eq!(store.last_processed_version().await.unwrap().get(), 5);

    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.through_event_version.get(), 5);
    assert_eq!(snapshot.monitor_health.len(), 1);
    assert_eq!(snapshot.monitor_health[0].component, "screen");
    assert!(!snapshot.monitor_health[0].healthy);
    assert_eq!(snapshot.monitor_health[0].reason_code, "capture_failed");
    assert_eq!(snapshot.monitor_health[0].last_event_version.get(), 5);
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.panes[0].pane, agent_pane());
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Busy);
    assert_eq!(
        snapshot.panes[0].work_summary.as_deref(),
        Some("Fix the projector")
    );
    assert_eq!(
        snapshot.panes[0].explicit_work_summary_updated_at_ms,
        Some(101)
    );

    let first = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(first.event_version.get(), 2);
    assert_eq!(first.event_type, "TurnCompleted");
    store.mark_delivered(&first.event_id).await.unwrap();
    let second = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(second.event_version.get(), 4);
    assert_eq!(second.event_type, "InboundMessageReceived");
    store.mark_delivered(&second.event_id).await.unwrap();
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn failed_projection_rolls_back_all_changes_and_replays_after_restart() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: agent_pane(),
        })],
    )
    .await
    .unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "replay me".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();

    store.fail_projection_before_checkpoint_for_test();
    assert!(store.project_unhandled_events(500).await.is_err());
    assert_eq!(store.last_processed_version().await.unwrap().get(), 0);
    assert!(store.load_agent_snapshot().await.unwrap().panes.is_empty());
    assert!(store.next_pending_delivery().await.unwrap().is_none());
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    let batch = reopened.project_unhandled_events(500).await.unwrap();
    assert_eq!(batch.applied, 2);
    assert_eq!(batch.through_event_version.get(), 2);
    assert_eq!(reopened.load_agent_snapshot().await.unwrap().panes.len(), 1);
    let mut sent = Vec::new();
    while let Some(pending) = reopened.next_pending_delivery().await.unwrap() {
        reopened.mark_delivered(&pending.event_id).await.unwrap();
        sent.push(pending.event_type);
    }
    assert_eq!(
        sent,
        ["InboundMessageReceived"],
        "the retried delivery is sent once"
    );
    assert_eq!(
        reopened
            .project_unhandled_events(500)
            .await
            .unwrap()
            .applied,
        0
    );
    assert!(reopened.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn projection_preserves_summary_candidates_across_restart_without_raw_screen_text() {
    const RAW_SCREEN_SENTINEL: &str = "RAW_SCREEN_SENTINEL_DO_NOT_STORE";

    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved { pane: agent_pane() }),
            AgentEvent::ScreenObserved(AgentScreenObserved {
                incarnation: agent_incarnation(),
                state: None,
                classifier_id: "screen-v1".into(),
                fallback_summary: Some(format!("\u{1b}P{RAW_SCREEN_SENTINEL}\u{1b}\\Review tests")),
                observed_at_ms: 110,
            }),
            AgentEvent::ScreenObserved(AgentScreenObserved {
                incarnation: agent_incarnation(),
                state: Some(ObservedAgentState::Idle),
                classifier_id: "screen-v1".into(),
                fallback_summary: None,
                observed_at_ms: 120,
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: agent_incarnation(),
                state: ObservedAgentState::Busy,
                adapter_id: "hook-v1".into(),
                work_summary: WorkSummaryUpdate::Set(format!(
                    "  Fix projection {}  ",
                    "🦀".repeat(200)
                )),
                observed_at_ms: 130,
            }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: agent_incarnation(),
                state: ObservedAgentState::Idle,
                adapter_id: "hook-v1".into(),
                work_summary: WorkSummaryUpdate::Clear,
                observed_at_ms: 140,
            }),
        ],
    )
    .await
    .unwrap();

    store.project_unhandled_events(500).await.unwrap();
    let snapshot = store.load_agent_snapshot().await.unwrap();
    let pane = &snapshot.panes[0];
    assert_eq!(pane.work_summary.as_deref(), Some("Review tests"));
    assert_eq!(pane.explicit_work_summary, None);
    assert_eq!(pane.explicit_work_summary_updated_at_ms, None);
    assert_eq!(pane.screen_work_summary.as_deref(), Some("Review tests"));
    assert_eq!(pane.screen_work_summary_updated_at_ms, Some(110));
    assert_eq!(pane.screen_state, None);
    assert_eq!(pane.screen_observed_at_ms, None);
    assert_eq!(pane.last_event_version.get(), 5);

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 500)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| !event.payload.to_string().contains(RAW_SCREEN_SENTINEL))
    );
    let explicit: AgentLifecycleObserved =
        serde_json::from_value(events[3].payload.clone()).unwrap();
    let WorkSummaryUpdate::Set(summary) = explicit.work_summary else {
        panic!("explicit summary should remain set");
    };
    assert!(summary.starts_with("Fix projection "));
    assert_eq!(summary.chars().count(), 160);
    let conn = store.state.connect().unwrap();
    let mut rows = conn
        .query(
            r#"
            SELECT COUNT(*)
            FROM agent_panes
            WHERE instr(pane_id, ?1) > 0
               OR instr(provider_id, ?1) > 0
               OR instr(tmux_target, ?1) > 0
               OR instr(session_name, ?1) > 0
               OR instr(working_directory, ?1) > 0
               OR instr(provider_display_name, ?1) > 0
               OR instr(screen_classifier_id, ?1) > 0
               OR instr(explicit_work_summary, ?1) > 0
               OR instr(screen_work_summary, ?1) > 0
               OR instr(work_summary, ?1) > 0
            "#,
            (RAW_SCREEN_SENTINEL,),
        )
        .await
        .unwrap();
    let count_value = rows.next().await.unwrap().unwrap().get_value(0).unwrap();
    assert_eq!(*count_value.as_integer().unwrap(), 0);
    assert!(
        conn.execute(
            "UPDATE agent_panes SET screen_work_summary = NULL WHERE pane_id = ?1",
            ("%7",),
        )
        .await
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE agent_panes SET work_summary = ?1 WHERE pane_id = ?2",
            ("x".repeat(161), "%7"),
        )
        .await
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE agent_panes SET screen_work_summary = ?1 WHERE pane_id = ?2",
            ("x".repeat(161), "%7"),
        )
        .await
        .is_err()
    );
    drop(conn);
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    let snapshot = reopened.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.through_event_version.get(), 5);
    assert_eq!(
        snapshot.panes[0].work_summary.as_deref(),
        Some("Review tests")
    );
    assert_eq!(
        snapshot.panes[0].screen_work_summary_updated_at_ms,
        Some(110)
    );
    drop(reopened);
    assert_persisted_files_exclude(directory.path(), RAW_SCREEN_SENTINEL.as_bytes());
}

#[tokio::test]
async fn configured_hook_grace_governs_replay_after_restart() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open_with_graces(
        directory.path(),
        crate::agent::reducer::GraceWindows::same(0),
    )
    .await
    .unwrap();
    append_agent_events(
        &store,
        vec![
            AgentEvent::PaneObserved(AgentPaneObserved { pane: agent_pane() }),
            AgentEvent::LifecycleObserved(AgentLifecycleObserved {
                incarnation: agent_incarnation(),
                state: ObservedAgentState::Busy,
                adapter_id: "hook-v1".into(),
                work_summary: WorkSummaryUpdate::Unchanged,
                observed_at_ms: 100,
            }),
            AgentEvent::ScreenObserved(AgentScreenObserved {
                incarnation: agent_incarnation(),
                state: Some(ObservedAgentState::Idle),
                classifier_id: "screen-v1".into(),
                fallback_summary: None,
                observed_at_ms: 101,
            }),
        ],
    )
    .await
    .unwrap();
    drop(store);

    let reopened = HaroldStore::open_with_graces(
        directory.path(),
        crate::agent::reducer::GraceWindows::same(0),
    )
    .await
    .unwrap();
    reopened.project_unhandled_events(500).await.unwrap();
    let snapshot = reopened.load_agent_snapshot().await.unwrap();
    assert_eq!(snapshot.panes[0].effective_state, EffectiveAgentState::Idle);
}

#[tokio::test]
async fn snapshot_reader_does_not_reserve_the_projection_writer_lock() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.path()).await.unwrap());
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "write while snapshot is open".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();

    let mut snapshot_gate = store.pause_snapshot_read_after_query_for_test();
    let reader_store = Arc::clone(&store);
    let reader = tokio::spawn(async move { reader_store.load_agent_snapshot().await });
    snapshot_gate.wait_until_started().await;

    let writer_store = Arc::clone(&store);
    let mut writer = tokio::spawn(async move { writer_store.project_unhandled_events(500).await });
    let batch = tokio::time::timeout(Duration::from_millis(500), &mut writer)
        .await
        .expect("read-only snapshot must not reserve the projection writer lock")
        .expect("projection task panicked")
        .expect("projection failed");
    assert_eq!(batch.applied, 1);

    snapshot_gate.resume();
    let snapshot = reader
        .await
        .expect("snapshot task panicked")
        .expect("snapshot read failed");
    assert_eq!(snapshot.through_event_version, EventVersion::start());
}

#[tokio::test]
async fn generated_activity_summary_survives_restart_and_projection_replay_without_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: agent_pane(),
        })],
    )
    .await
    .unwrap();
    store
        .stream()
        .append([NewEvent::new(
            "AgentActivitySummaryGenerated",
            json!({
                "incarnation": agent_incarnation(),
                "basis_version": 1,
                "description": "Reviewing projection persistence",
                "generated_at_ms": 120
            }),
        )])
        .await
        .unwrap();
    store.fail_projection_before_checkpoint_for_test();
    assert!(store.project_unhandled_events(500).await.is_err());
    assert!(store.load_agent_snapshot().await.unwrap().panes.is_empty());
    assert_eq!(
        store.last_processed_version().await.unwrap(),
        EventVersion::start()
    );
    assert!(store.next_pending_delivery().await.unwrap().is_none());
    store.project_unhandled_events(500).await.unwrap();
    let projected = store.load_agent_snapshot().await.unwrap();
    assert_eq!(
        projected.panes[0].work_summary.as_deref(),
        Some("Reviewing projection persistence")
    );
    assert_eq!(projected.panes[0].summary_basis_version.get(), 1);
    assert_eq!(
        projected.panes[0]
            .generated_summary_basis_version
            .unwrap()
            .get(),
        1
    );
    assert_eq!(
        projected.panes[0].generated_work_summary,
        projected.panes[0].work_summary
    );
    assert!(store.next_pending_delivery().await.unwrap().is_none());
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    assert_eq!(
        reopened.load_agent_snapshot().await.unwrap().panes,
        projected.panes
    );
    drop(reopened);
    for suffix in ["", "-shm", "-wal"] {
        let path = directory.path().join(format!("harold-state.db{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let replayed = HaroldStore::open(directory.path()).await.unwrap();
    replayed.project_unhandled_events(500).await.unwrap();
    assert_eq!(
        replayed.load_agent_snapshot().await.unwrap().panes,
        projected.panes
    );
    assert!(replayed.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn generated_activity_summary_append_normalizes_description_and_rejects_empty() {
    use crate::agent::domain::AgentActivitySummaryGenerated;

    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let generated = |description: String| {
        AgentEvent::ActivitySummaryGenerated(AgentActivitySummaryGenerated {
            incarnation: agent_incarnation(),
            basis_version: EventVersion::new(1).unwrap(),
            description,
            generated_at_ms: 120,
        })
    };
    let appended = append_agent_events(
        &store,
        vec![
            generated(" \u{1b}[31mReviewing\n projections ".into()),
            generated(" \u{1b}[31m\n\t".into()),
            generated("x".repeat(200)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(appended.events.len(), 2);
    let stored = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].r#type, "AgentActivitySummaryGenerated");
    let event: AgentActivitySummaryGenerated =
        serde_json::from_value(stored[0].payload.clone()).unwrap();
    assert_eq!(event.description, "Reviewing projections");
    assert_eq!(event.incarnation, agent_incarnation());
    assert_eq!(event.basis_version.get(), 1);
    assert_eq!(event.generated_at_ms, 120);
    let event: AgentActivitySummaryGenerated =
        serde_json::from_value(stored[1].payload.clone()).unwrap();
    assert_eq!(event.description.chars().count(), 160);
}

#[tokio::test]
async fn an_empty_folder_becomes_a_new_store_with_the_defaults() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    assert!(store.is_new());
    assert!(directory.path().join("log").is_dir());
    assert!(directory.path().join("harold-state.db").is_file());
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert!(!snapshot.tts_muted, "voice is on");
    assert!(!snapshot.messaging_paused, "messaging is unpaused");
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn an_existing_store_is_opened_as_it_is() {
    let directory = TestDirectory::new();
    {
        let store = HaroldStore::open(directory.path()).await.unwrap();
        append_tts_mute_changed(&store, true).await.unwrap();
        store.project_unhandled_events(10).await.unwrap();
    }
    let store = HaroldStore::open(directory.path()).await.unwrap();
    assert!(!store.is_new());
    assert!(store.load_agent_snapshot().await.unwrap().tts_muted);
}

#[tokio::test]
async fn a_state_database_that_has_read_past_the_log_is_refused() {
    let directory = TestDirectory::new();
    let cursor = {
        let store = HaroldStore::open(directory.path()).await.unwrap();
        append_tts_mute_changed(&store, true).await.unwrap();
        store.project_unhandled_events(10).await.unwrap();
        store.last_processed_version().await.unwrap()
    };
    assert!(!cursor.is_start());
    std::fs::remove_dir_all(directory.path().join("log")).unwrap();
    std::fs::remove_file(directory.path().join(super::event_log::LOG_SETTINGS_FILE)).unwrap();

    let Err(error) = HaroldStore::open(directory.path()).await else {
        panic!("a state database ahead of its log must be refused");
    };
    assert_eq!(
        error.to_string(),
        format!(
            "harold-state.db has read the event log up to version {cursor}, but the log in {} \
             ends before it; the store folder is inconsistent",
            directory.path().join("log").display()
        )
    );
}

#[tokio::test]
async fn tts_mute_is_projected_into_the_snapshot_and_survives_reopen() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    assert!(!store.load_agent_snapshot().await.unwrap().tts_muted);

    append_tts_mute_changed(&store, true).await.unwrap();
    let batch = store.project_unhandled_events(10).await.unwrap();
    assert_eq!(batch.applied, 1);
    assert!(batch.snapshot_changed);
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert!(snapshot.tts_muted);
    assert_eq!(snapshot.through_event_version, batch.through_event_version);

    drop(store);
    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    assert!(reopened.load_agent_snapshot().await.unwrap().tts_muted);

    append_tts_mute_changed(&reopened, false).await.unwrap();
    reopened.project_unhandled_events(10).await.unwrap();
    assert!(!reopened.load_agent_snapshot().await.unwrap().tts_muted);
}

#[tokio::test]
async fn tts_mute_event_creates_no_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_tts_mute_changed(&store, true).await.unwrap();
    store.project_unhandled_events(10).await.unwrap();
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn messaging_pause_is_projected_into_the_snapshot_and_survives_reopen() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    assert!(!store.load_agent_snapshot().await.unwrap().messaging_paused);

    append_messaging_paused_changed(&store, true).await.unwrap();
    let batch = store.project_unhandled_events(10).await.unwrap();
    assert_eq!(batch.applied, 1);
    assert!(batch.snapshot_changed);
    let snapshot = store.load_agent_snapshot().await.unwrap();
    assert!(snapshot.messaging_paused);
    assert!(!snapshot.tts_muted);
    assert_eq!(snapshot.through_event_version, batch.through_event_version);

    drop(store);
    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    assert!(
        reopened
            .load_agent_snapshot()
            .await
            .unwrap()
            .messaging_paused
    );

    append_messaging_paused_changed(&reopened, false)
        .await
        .unwrap();
    reopened.project_unhandled_events(10).await.unwrap();
    assert!(
        !reopened
            .load_agent_snapshot()
            .await
            .unwrap()
            .messaging_paused
    );
}

#[tokio::test]
async fn messaging_pause_event_creates_no_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_messaging_paused_changed(&store, true).await.unwrap();
    store.project_unhandled_events(10).await.unwrap();
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[test]
fn inbound_message_without_the_forwarded_field_reads_as_not_forwarded() {
    // The shape of every InboundMessageReceived event stored before the field existed.
    let message: InboundMessage = serde_json::from_value(json!({ "text": "carry on" })).unwrap();

    assert_eq!(message.text, "carry on");
    assert!(!message.forwarded);
}

#[tokio::test]
async fn forwarded_flag_reaches_the_pending_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_inbound_message(
        &store,
        &InboundMessage {
            text: "carry on".into(),
            forwarded: true,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(10).await.unwrap();

    let delivery = store.next_pending_delivery().await.unwrap().unwrap();
    let message: InboundMessage = serde_json::from_value(delivery.payload).unwrap();

    assert_eq!(message.text, "carry on");
    assert!(message.forwarded);
}

#[tokio::test]
async fn a_store_made_by_v0_3_1_is_refused() {
    let directory = TestDirectory::new();
    let state_path = directory.path().join(super::STATE_DATABASE);
    let state = turso::Builder::new_local(state_path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = state.connect().unwrap();
    conn.execute(
        r#"
        CREATE TABLE _migrations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            checksum TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL
        )
        "#,
        (),
    )
    .await
    .unwrap();
    for (name, checksum) in [
        (
            "001_initial",
            "bc4ff7edbea046516df88dc4fedf03b29b442d977ee69b52290fc4b0a36d42ff",
        ),
        (
            "002_settings",
            "0da3944133e15c66194bae29cf350dc82164062a6982d7d1d19573c6bb21553d",
        ),
    ] {
        conn.execute(
            "INSERT INTO _migrations (name, checksum, applied_at_ms) VALUES (?1, ?2, 0)",
            (name, checksum),
        )
        .await
        .unwrap();
    }
    drop(conn);
    drop(state);

    let error = HaroldStore::open(directory.path())
        .await
        .err()
        .expect("a v0.3.1 store must be refused");
    assert!(error.to_string().contains("incompatible state schema"));
}

#[tokio::test]
async fn the_reporting_adapter_is_stored_with_the_pane_and_survives_reopen() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_agent_events(
        &store,
        vec![AgentEvent::PaneObserved(AgentPaneObserved {
            pane: agent_pane(),
        })],
    )
    .await
    .unwrap();
    store.project_unhandled_events(500).await.unwrap();
    let before = store.load_agent_snapshot().await.unwrap();
    assert_eq!(before.panes[0].lifecycle_adapter_id, None);

    append_agent_events(
        &store,
        vec![AgentEvent::LifecycleObserved(AgentLifecycleObserved {
            incarnation: agent_incarnation(),
            state: ObservedAgentState::Busy,
            adapter_id: "claude.lifecycle".into(),
            work_summary: WorkSummaryUpdate::Unchanged,
            observed_at_ms: 100,
        })],
    )
    .await
    .unwrap();
    store.project_unhandled_events(500).await.unwrap();
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    let after = reopened.load_agent_snapshot().await.unwrap();
    assert_eq!(
        after.panes[0].lifecycle_adapter_id.as_deref(),
        Some("claude.lifecycle")
    );
}

#[test]
fn the_store_takes_both_windows_from_the_monitor_settings() {
    let monitor = crate::settings::AgentMonitorSettings {
        hook_grace_ms: 1_234,
        report_grace_ms: 56_789,
        ..crate::settings::AgentMonitorSettings::default()
    };

    let graces = super::graces_from(&monitor);

    assert_eq!(graces.hook_ms, 1_234);
    assert_eq!(graces.report_ms, 56_789);
}

fn timed_prompt_typed(name: &str, day: &str) -> TimedPromptHandled {
    TimedPromptHandled {
        name: name.into(),
        day: day.into(),
        outcome: TimedPromptOutcome::Typed,
        reason: None,
        pane_id: Some("%2".into()),
    }
}

#[tokio::test]
async fn timed_prompt_event_is_stored_with_the_payload_the_spec_gives() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    append_timed_prompt_handled(&store, &timed_prompt_typed("digest", "2026-10-14"))
        .await
        .unwrap();
    append_timed_prompt_handled(
        &store,
        &TimedPromptHandled {
            name: "digest".into(),
            day: "2026-10-15".into(),
            outcome: TimedPromptOutcome::Skipped,
            reason: Some("agent_busy".into()),
            pane_id: None,
        },
    )
    .await
    .unwrap();

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].r#type.as_str(), "TimedPromptHandled");
    assert_eq!(
        events[0].payload,
        json!({"name": "digest", "day": "2026-10-14", "outcome": "typed", "reason": null, "pane_id": "%2"})
    );
    assert_eq!(
        events[1].payload,
        json!({"name": "digest", "day": "2026-10-15", "outcome": "skipped", "reason": "agent_busy", "pane_id": null})
    );
    assert_eq!(
        serde_json::to_value(TimedPromptOutcome::Failed).unwrap(),
        json!("failed")
    );
}

#[tokio::test]
async fn timed_prompt_event_is_projected_into_one_row_for_each_name_and_creates_no_delivery() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    assert!(store.timed_prompt_last_days().await.unwrap().is_empty());

    append_timed_prompt_handled(&store, &timed_prompt_typed("digest", "2026-10-14"))
        .await
        .unwrap();
    append_timed_prompt_handled(&store, &timed_prompt_typed("stand-up", "2026-10-14"))
        .await
        .unwrap();
    append_timed_prompt_handled(&store, &timed_prompt_typed("digest", "2026-10-15"))
        .await
        .unwrap();
    // The mute setting shares the table and is not a timed prompt row.
    append_tts_mute_changed(&store, true).await.unwrap();
    let batch = store.project_unhandled_events(10).await.unwrap();

    assert_eq!(batch.applied, 4);
    let days = store.timed_prompt_last_days().await.unwrap();
    assert_eq!(days.len(), 2, "{days:?}");
    assert_eq!(days["digest"], "2026-10-15");
    assert_eq!(days["stand-up"], "2026-10-14");
    assert!(store.next_pending_delivery().await.unwrap().is_none());

    drop(store);
    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    assert_eq!(reopened.timed_prompt_last_days().await.unwrap(), days);
}

#[tokio::test]
async fn replaying_the_stream_into_a_fresh_state_database_reproduces_the_timed_prompt_rows() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_timed_prompt_handled(&store, &timed_prompt_typed("digest", "2026-10-14"))
        .await
        .unwrap();
    append_timed_prompt_handled(&store, &timed_prompt_typed("stand-up", "2026-10-15"))
        .await
        .unwrap();
    store.project_unhandled_events(10).await.unwrap();
    let projected = store.timed_prompt_last_days().await.unwrap();
    assert_eq!(projected.len(), 2);
    drop(store);

    for suffix in ["", "-shm", "-wal"] {
        let path = directory.path().join(format!("harold-state.db{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let replayed = HaroldStore::open(directory.path()).await.unwrap();
    assert!(replayed.timed_prompt_last_days().await.unwrap().is_empty());
    replayed.project_unhandled_events(500).await.unwrap();
    assert_eq!(replayed.timed_prompt_last_days().await.unwrap(), projected);
}

#[tokio::test]
async fn timed_prompt_event_with_an_unreadable_payload_does_not_block_later_events() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    store
        .stream()
        .append([NewEvent::new("TimedPromptHandled", json!({"name": 7}))])
        .await
        .unwrap();
    append_timed_prompt_handled(&store, &timed_prompt_typed("digest", "2026-10-14"))
        .await
        .unwrap();

    let batch = store.project_unhandled_events(10).await.unwrap();

    assert_eq!(batch.applied, 2);
    let days = store.timed_prompt_last_days().await.unwrap();
    assert_eq!(days.len(), 1);
    assert_eq!(days["digest"], "2026-10-14");
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn timed_prompt_append_fails_once_when_the_test_asks_for_it() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    store.fail_next_monitor_append_for_test();

    let event = timed_prompt_typed("digest", "2026-10-14");
    assert!(append_timed_prompt_handled(&store, &event).await.is_err());
    assert!(append_timed_prompt_handled(&store, &event).await.is_ok());
}

/// `target` as a path relative to `base`; both absolute.
fn relative_to(base: &Path, target: &Path) -> PathBuf {
    let base: Vec<_> = base.components().collect();
    let target: Vec<_> = target.components().collect();
    let shared = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for part in &target[shared..] {
        relative.push(part);
    }
    relative
}

#[test]
fn log_config_uses_absolute_local_paths_and_a_harold_settings_file() {
    let directory = TestDirectory::new();
    // A relative path, as `[store] path` may be in development.
    let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let relative = relative_to(&cwd, &std::fs::canonicalize(directory.path()).unwrap());
    assert!(relative.is_relative());
    let config = super::event_log::log_config(&relative).expect("config");
    let opendata_common::StorageConfig::SlateDb(slate) = config.storage else {
        panic!("Harold's log is on SlateDB");
    };
    let opendata_common::ObjectStoreConfig::Local(local) = slate.object_store else {
        panic!("Harold's log is on the local filesystem");
    };
    let log_dir = std::fs::canonicalize(directory.path()).unwrap().join("log");
    assert_eq!(Path::new(&local.path), log_dir);
    let settings = slate.settings_path.expect("an explicit settings path");
    assert!(Path::new(&settings).is_absolute());
    assert_eq!(
        Path::new(&settings),
        std::fs::canonicalize(directory.path())
            .unwrap()
            .join("log-settings.toml")
    );
    assert_eq!(config.read_visibility, opendata_log::ReadVisibility::Remote);
    assert!(config.retention.retention.is_none());
}

#[tokio::test]
async fn an_edited_settings_file_is_rewritten_on_open() {
    let directory = TestDirectory::new();
    drop(HaroldStore::open(directory.path()).await.unwrap());
    let settings = directory.path().join("log-settings.toml");
    std::fs::write(&settings, "[not]\nharold = true\n").unwrap();
    drop(HaroldStore::open(directory.path()).await.unwrap());
    assert!(
        !std::fs::read_to_string(&settings)
            .unwrap()
            .contains("harold = true")
    );
}

#[tokio::test]
async fn event_versions_stay_within_their_bounds_and_a_fresh_store_starts_at_one() {
    assert!(EventVersion::new(0).is_err());
    assert!(EventVersion::new(i64::MAX as u64 + 1).is_err());
    assert!(EventVersion::from_column(0).unwrap().is_start());
    assert!(EventVersion::from_column(-1).is_err());

    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let appended = store
        .stream()
        .append([NewEvent::new("TtsMuteChanged", json!({"muted": true}))])
        .await
        .unwrap();
    assert_eq!(appended.first_version.get(), 1);
    let read = store
        .stream()
        .read(appended.first_version)
        .await
        .unwrap()
        .expect("the appended event reads back");
    assert_eq!(read.id, appended.events[0].id);
    assert_eq!(read.created_at, appended.events[0].created_at);
}

#[tokio::test]
async fn a_record_with_a_time_out_of_range_reads_as_undecodable_and_later_events_still_load() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let raw = format!(
        r#"{{"id":"{}","type":"X","payload":{{}},"created_at_ms":9223372036854775807,"trace_id":null,"span_id":null,"actor":"a"}}"#,
        uuid::Uuid::new_v4()
    );
    let raw_version = store
        .stream()
        .append_raw_for_test(raw.into_bytes())
        .await
        .unwrap();
    append_tts_mute_changed(&store, true).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].version, raw_version);
    assert_eq!(events[0].r#type, "UndecodableEvent");
    assert_eq!(events[1].r#type, "TtsMuteChanged");
    let read = store.stream().read(raw_version).await.unwrap().unwrap();
    assert_eq!(read.r#type, "UndecodableEvent");
    assert_eq!(read.id, events[0].id);
}

#[tokio::test]
async fn a_second_store_on_the_same_folder_is_refused_and_the_first_keeps_working() {
    let directory = TestDirectory::new();
    let first = HaroldStore::open(directory.path()).await.unwrap();
    let Err(error) = HaroldStore::open(directory.path()).await else {
        panic!("a second store on the same folder must be refused");
    };
    assert_eq!(error.to_string(), "store is in use by another Harold");
    append_tts_mute_changed(&first, true).await.unwrap();
    let batch = first.project_unhandled_events(10).await.unwrap();
    assert_eq!(batch.applied, 1);
    assert!(first.load_agent_snapshot().await.unwrap().tts_muted);
}

#[tokio::test]
async fn a_dropped_store_releases_the_folder() {
    let directory = TestDirectory::new();
    drop(HaroldStore::open(directory.path()).await.unwrap());
    HaroldStore::open(directory.path())
        .await
        .expect("reopened after drop");
}

#[tokio::test]
async fn an_event_survives_reopening_and_later_versions_are_higher() {
    let directory = TestDirectory::new();
    let mut versions = Vec::new();
    for muted in [true, false, true] {
        let store = HaroldStore::open(directory.path()).await.unwrap();
        append_tts_mute_changed(&store, muted).await.unwrap();
        let events = store
            .stream()
            .load_after_version(EventVersion::start(), 10)
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            versions.len() + 1,
            "earlier events are all still there"
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.version)
                .take(versions.len())
                .collect::<Vec<_>>(),
            versions
        );
        let newest = events.last().unwrap().version;
        assert!(versions.iter().all(|earlier| newest > *earlier));
        versions.push(newest);
    }
}

#[tokio::test]
async fn concurrent_appends_are_projected_once_in_version_order() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.path()).await.unwrap());
    let tasks: Vec<_> = (0..50)
        .map(|n| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                append_inbound_message(
                    &store,
                    &InboundMessage {
                        text: format!("m{n}"),
                        forwarded: false,
                    },
                )
                .await
                .unwrap();
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let mut applied = 0;
    loop {
        let batch = store.project_unhandled_events(7).await.unwrap();
        if batch.applied == 0 {
            break;
        }
        applied += batch.applied;
    }
    assert_eq!(applied, 50);
    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap();
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].version < pair[1].version)
    );
    assert_eq!(
        store.last_processed_version().await.unwrap(),
        events.last().unwrap().version
    );
    let mut delivered = 0;
    while let Some(pending) = store.next_pending_delivery().await.unwrap() {
        store.mark_delivered(&pending.event_id).await.unwrap();
        delivered += 1;
    }
    assert_eq!(delivered, 50);
}

#[tokio::test]
async fn an_event_appended_inside_a_span_stores_its_trace_and_span_ids() {
    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use tracing::Instrument;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("store-test")));
    let _guard = tracing::subscriber::set_default(subscriber);

    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_tts_mute_changed(&store, false).await.unwrap();
    let span = tracing::info_span!("append");
    let expected = span.context().span().span_context().clone();
    append_tts_mute_changed(&store, true)
        .instrument(span)
        .await
        .unwrap();

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(
        events[0].trace_id, None,
        "outside a span both ids are empty"
    );
    assert_eq!(events[0].span_id, None);
    assert_eq!(events[1].trace_id, Some(expected.trace_id().to_string()));
    assert_eq!(events[1].span_id, Some(expected.span_id().to_string()));
}

/// Whether `text` calls `function` by its bare name: `function(` that is not a method call
/// (`.function(`), a definition (`fn function(`), a path (`time::function(`, checked
/// separately) or the end of a longer name.
fn calls_bare(text: &str, function: &str) -> bool {
    text.match_indices(&format!("{function}(")).any(|(at, _)| {
        let before = &text[..at];
        let joined = before
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':'));
        !joined && !before.ends_with("fn ")
    })
}

#[test]
fn no_code_outside_tests_pauses_or_advances_tokio_time() {
    // opendata-log turns on tokio's `test-util`, so a stray pause would compile into the
    // daemon. Test files are `*_tests.rs` and `tests/`; everything else is daemon code.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut offenders = Vec::new();
    for crate_dir in ["harold/src", "harold-api/src", "tmx-agent-dash/src"] {
        let mut pending = vec![workspace.join(crate_dir)];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy();
                if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for needle in ["time::pause", "time::advance", "start_paused"] {
                    if text.contains(needle) {
                        offenders.push(format!("{} uses {needle}", path.display()));
                    }
                }
                // After `use tokio::time::{advance, pause}`, the calls are bare.
                for function in ["pause", "advance"] {
                    if calls_bare(&text, function) {
                        offenders.push(format!("{} calls {function}(", path.display()));
                    }
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:?}");
}

// ---------------------------------------------------------------------------
// A failing store
// ---------------------------------------------------------------------------

/// A store write: the smallest append there is.
async fn write(store: &HaroldStore) -> super::Result<()> {
    append_tts_mute_changed(store, true).await
}

/// Marks `store` failing with an append whose flush never returns, and leaves the fault on.
async fn time_out_one_write(store: &HaroldStore) {
    store.fault_log_flush_for_test(Some(WriteFault::Hang));
    let result = write(store).await;
    assert!(
        matches!(result, Err(StoreError::Failing(FailedWrite::TimedOut))),
        "{result:?}"
    );
}

fn is_refused(result: &super::Result<()>) -> bool {
    matches!(result, Err(StoreError::Failing(FailedWrite::Refused)))
}

#[tokio::test(start_paused = true)]
async fn an_append_whose_flush_never_returns_fails_after_30_seconds_and_marks_the_store_failing() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    store.fault_log_flush_for_test(Some(WriteFault::Hang));
    let started = Instant::now();

    let result = write(&store).await;

    assert!(
        matches!(result, Err(StoreError::Failing(FailedWrite::TimedOut))),
        "{result:?}"
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(30) && waited < Duration::from_secs(31),
        "{waited:?}"
    );
    assert!(store.is_failing());
}

#[tokio::test(start_paused = true)]
async fn a_failing_store_refuses_writes_at_once_then_lets_one_try_through_after_30_seconds() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    time_out_one_write(&store).await;
    store.fault_log_flush_for_test(None);

    let started = Instant::now();
    let refused = write(&store).await;
    assert!(is_refused(&refused), "{refused:?}");
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(store.is_failing());

    tokio::time::advance(Duration::from_secs(29)).await;
    let refused = write(&store).await;
    assert!(is_refused(&refused), "{refused:?}");

    tokio::time::advance(Duration::from_secs(1)).await;
    write(&store).await.unwrap();
    assert!(!store.is_failing());
    write(&store).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_try_that_times_out_keeps_the_store_failing_for_another_30_seconds() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    time_out_one_write(&store).await;

    tokio::time::advance(Duration::from_secs(30)).await;
    let started = Instant::now();
    let tried = write(&store).await;
    assert!(
        matches!(tried, Err(StoreError::Failing(FailedWrite::TimedOut))),
        "{tried:?}"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));
    let refused = write(&store).await;
    assert!(is_refused(&refused), "{refused:?}");
    assert!(store.is_failing());

    tokio::time::advance(Duration::from_secs(30)).await;
    store.fault_log_flush_for_test(None);
    write(&store).await.unwrap();
    assert!(!store.is_failing());
}

/// Lets a spawned task run until it waits.
async fn let_run() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_write_while_a_try_is_in_flight_is_refused_at_once() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.path()).await.unwrap());
    time_out_one_write(&store).await;
    tokio::time::advance(Duration::from_secs(30)).await;

    let trying = Arc::clone(&store);
    let attempt = tokio::spawn(async move { write(&trying).await });
    let_run().await;
    let started = Instant::now();
    let refused = write(&store).await;
    assert!(is_refused(&refused), "{refused:?}");
    assert_eq!(started.elapsed(), Duration::ZERO);

    // A hang never ends, even once the fault is cleared, so the try times out.
    let tried = attempt.await.unwrap();
    assert!(
        matches!(tried, Err(StoreError::Failing(FailedWrite::TimedOut))),
        "{tried:?}"
    );
    assert!(store.is_failing());
}

#[tokio::test(start_paused = true)]
async fn a_dropped_try_lets_the_next_write_try_30_seconds_later() {
    let directory = TestDirectory::new();
    let store = Arc::new(HaroldStore::open(directory.path()).await.unwrap());
    time_out_one_write(&store).await;
    tokio::time::advance(Duration::from_secs(30)).await;

    let trying = Arc::clone(&store);
    let attempt = tokio::spawn(async move { write(&trying).await });
    let_run().await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());

    // The dropped try counts as a try: the next one is 30 seconds later.
    let refused = write(&store).await;
    assert!(is_refused(&refused), "{refused:?}");
    tokio::time::advance(Duration::from_secs(30)).await;
    store.fault_log_flush_for_test(None);
    write(&store).await.unwrap();
    assert!(!store.is_failing());
}

#[tokio::test(start_paused = true)]
async fn a_healthy_append_that_fails_without_timing_out_keeps_the_store_healthy() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    store.fault_log_flush_for_test(Some(WriteFault::Fail));

    let result = write(&store).await;

    assert!(matches!(result, Err(StoreError::Log(_))), "{result:?}");
    assert!(!store.is_failing());
    store.fault_log_flush_for_test(None);
    write(&store).await.unwrap();
}

/// The line of `text` that holds `message`.
fn line_with<'a>(text: &'a str, message: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(message))
        .unwrap_or_else(|| panic!("no {message:?} line in {text}"))
}

#[tokio::test(start_paused = true)]
async fn the_store_failing_and_recovered_lines_carry_only_component_and_reason() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let logs = crate::outbound::fake::CapturedLogs::start();

    let span = tracing::info_span!("caller", trace_id = "not-in-the-line");
    let _entered = span.enter();
    time_out_one_write(&store).await;
    store.fault_log_flush_for_test(None);
    tokio::time::advance(Duration::from_secs(30)).await;
    write(&store).await.unwrap();

    let text = logs.text();
    let failing = line_with(&text, "store failing");
    assert!(failing.contains("WARN"), "{failing}");
    assert!(failing.contains("component=\"store\""), "{failing}");
    assert!(failing.contains("reason=\"write_timed_out\""), "{failing}");
    assert_eq!(failing.matches('=').count(), 2, "{failing}");
    let recovered = line_with(&text, "store recovered");
    assert!(recovered.contains("INFO"), "{recovered}");
    assert!(recovered.contains("component=\"store\""), "{recovered}");
    assert!(
        recovered.contains("reason=\"try_succeeded\""),
        "{recovered}"
    );
    assert_eq!(recovered.matches('=').count(), 2, "{recovered}");
    assert_eq!(text.matches("store failing").count(), 1, "{text}");
}

/// The lines queued so far.
fn queued(alerts: &mut tokio::sync::mpsc::Receiver<crate::alerts::Queued>) -> Vec<String> {
    std::iter::from_fn(|| alerts.try_recv().ok().map(|(line, _)| line)).collect()
}

#[tokio::test(start_paused = true)]
async fn the_store_failing_queues_firing_once_and_recovering_queues_cleared_once() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let (queue, mut alerts) = crate::alerts::AlertQueue::new();
    store.set_alert_queue(queue);
    store.fault_log_flush_for_test(Some(WriteFault::Hang));

    let (first, second) = tokio::join!(write(&store), write(&store));
    for result in [first, second] {
        assert!(
            matches!(result, Err(StoreError::Failing(FailedWrite::TimedOut))),
            "{result:?}"
        );
    }
    let refused = write(&store).await;
    assert!(is_refused(&refused), "{refused:?}");
    assert_eq!(queued(&mut alerts), ["harold alert: store-failing firing"]);

    store.fault_log_flush_for_test(None);
    tokio::time::advance(Duration::from_secs(30)).await;
    write(&store).await.unwrap();
    assert_eq!(queued(&mut alerts), ["harold alert: store-failing cleared"]);
}

#[tokio::test(start_paused = true)]
async fn a_failing_spell_before_the_queue_is_set_queues_no_cleared_line() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    time_out_one_write(&store).await;
    let (queue, mut alerts) = crate::alerts::AlertQueue::new();
    store.set_alert_queue(queue);

    store.fault_log_flush_for_test(None);
    tokio::time::advance(Duration::from_secs(30)).await;
    write(&store).await.unwrap();

    assert!(queued(&mut alerts).is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_empty_append_writes_nothing_so_it_is_not_a_try() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    time_out_one_write(&store).await;
    store.fault_log_flush_for_test(None);
    tokio::time::advance(Duration::from_secs(30)).await;

    let appended = append_agent_events(&store, Vec::new()).await.unwrap();

    assert!(appended.events.is_empty());
    assert!(store.is_failing());
    write(&store).await.unwrap();
    assert!(!store.is_failing());
}

/// How many `delivery_outbox` rows there are, delivered or not.
async fn outbox_row_count(store: &HaroldStore) -> i64 {
    let conn = store.state.connect().unwrap();
    let mut rows = conn
        .query("SELECT COUNT(*) FROM delivery_outbox", ())
        .await
        .unwrap();
    *rows
        .next()
        .await
        .unwrap()
        .unwrap()
        .get_value(0)
        .unwrap()
        .as_integer()
        .unwrap()
}

/// Whether the state database has a table named `name`.
async fn has_table(conn: &turso::Connection, name: &str) -> bool {
    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            (name,),
        )
        .await
        .unwrap();
    rows.next().await.unwrap().is_some()
}

fn push_subscription(endpoint: &str, keys: &str) -> AppPushSubscribed {
    AppPushSubscribed {
        endpoint: endpoint.into(),
        p256dh: format!("p256dh-{keys}"),
        auth: format!("auth-{keys}"),
    }
}

fn texts_of(messages: &[AppMessage]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message.text.as_str())
        .collect()
}

#[tokio::test]
async fn an_app_reply_is_stored_with_to_chief_and_projected_as_a_user_row() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    append_app_reply(&store, "hi").await.unwrap();
    store.project_unhandled_events(10).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.r#type, "InboundMessageReceived");
    let reply: ReceivedReply = serde_json::from_value(event.payload.clone()).unwrap();
    assert_eq!(reply.text, "hi");
    assert!(reply.to_chief);
    assert!(!reply.forwarded);

    let messages = store.app_messages_since(0).await.unwrap();
    assert_eq!(
        messages,
        [AppMessage {
            event_id: event.id.to_string(),
            at_ms: (event.created_at.unix_timestamp_nanos() / 1_000_000) as i64,
            sender: AppSender::User,
            text: "hi".into(),
            state: None,
        }]
    );

    let pending = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(pending.event_type, "InboundMessageReceived");
    assert_eq!(pending.event_id, event.id.to_string());
    assert_eq!(outbox_row_count(&store).await, 1);
}

#[tokio::test]
async fn an_inbound_message_without_to_chief_reads_as_false_and_adds_no_row() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    append_inbound_message(
        &store,
        &InboundMessage {
            text: "x".into(),
            forwarded: false,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(10).await.unwrap();

    let events = store
        .stream()
        .load_after_version(EventVersion::start(), 10)
        .await
        .unwrap();
    let reply: ReceivedReply = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(reply.text, "x");
    assert!(!reply.to_chief);
    assert!(store.app_messages_since(0).await.unwrap().is_empty());
}

#[tokio::test]
async fn push_subscriptions_follow_their_events() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    append_app_push_subscribed(&store, &push_subscription("https://push/a", "a1"))
        .await
        .unwrap();
    append_app_push_subscribed(&store, &push_subscription("https://push/b", "b1"))
        .await
        .unwrap();
    append_app_push_subscribed(&store, &push_subscription("https://push/a", "a2"))
        .await
        .unwrap();
    append_app_push_unsubscribed(
        &store,
        &AppPushUnsubscribed {
            endpoint: "https://push/b".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 4);

    assert_eq!(
        store.app_push_subscriptions().await.unwrap(),
        [push_subscription("https://push/a", "a2")]
    );
    assert_eq!(outbox_row_count(&store).await, 0);
}

#[tokio::test]
async fn a_chief_message_is_added_once_and_a_reply_state_is_shown() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    store.add_chief_app_message("e1", 10, "m").await.unwrap();
    store.add_chief_app_message("e1", 10, "m").await.unwrap();
    assert_eq!(
        store.app_messages_since(0).await.unwrap(),
        [AppMessage {
            event_id: "e1".into(),
            at_ms: 10,
            sender: AppSender::Chief,
            text: "m".into(),
            state: None,
        }]
    );

    append_app_reply(&store, "on it?").await.unwrap();
    store.project_unhandled_events(10).await.unwrap();
    let reply_id = store
        .app_messages_since(0)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.sender == AppSender::User)
        .unwrap()
        .event_id;
    store
        .set_app_reply_state(&reply_id, AppReplyState::WaitingChiefBusy)
        .await
        .unwrap();

    let reply = store
        .app_messages_since(0)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.event_id == reply_id)
        .unwrap();
    assert_eq!(reply.state, Some(AppReplyState::WaitingChiefBusy));
}

#[test]
fn reply_states_carry_the_labels_the_app_shows() {
    assert_eq!(AppReplyState::Typed.label(), "typed");
    assert_eq!(
        AppReplyState::WaitingChiefBusy.label(),
        "waiting: chief busy"
    );
    assert_eq!(AppReplyState::NotSentNoChief.label(), "not sent: no chief");
    assert_eq!(
        AppReplyState::NotSentPaused.label(),
        "not sent: messaging paused"
    );
}

#[tokio::test]
async fn thread_rows_come_oldest_first_and_old_ones_are_deleted() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();

    store
        .add_chief_app_message("e30", 30, "thirty")
        .await
        .unwrap();
    store.add_chief_app_message("e10", 10, "ten").await.unwrap();
    store
        .add_chief_app_message("e20", 20, "twenty")
        .await
        .unwrap();
    assert_eq!(
        texts_of(&store.app_messages_since(0).await.unwrap()),
        ["ten", "twenty", "thirty"]
    );
    assert_eq!(
        texts_of(&store.app_messages_since(20).await.unwrap()),
        ["twenty", "thirty"]
    );

    // Equal times keep insertion order, whatever the ids sort as.
    store.add_chief_app_message("z", 40, "first").await.unwrap();
    store
        .add_chief_app_message("a", 40, "second")
        .await
        .unwrap();
    assert_eq!(
        texts_of(&store.app_messages_since(40).await.unwrap()),
        ["first", "second"]
    );

    store.delete_app_messages_before(25).await.unwrap();
    assert_eq!(
        texts_of(&store.app_messages_since(0).await.unwrap()),
        ["thirty", "first", "second"]
    );
}

#[tokio::test]
async fn the_thread_watch_changes_on_each_change() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    let mut thread = store.watch_app_thread();
    let changed = |thread: &mut tokio::sync::watch::Receiver<u64>| {
        let saw = thread.has_changed().unwrap();
        thread.borrow_and_update();
        saw
    };
    assert!(!changed(&mut thread));

    append_app_reply(&store, "hi").await.unwrap();
    store.project_unhandled_events(10).await.unwrap();
    assert!(changed(&mut thread), "the projection added a user row");

    store.add_chief_app_message("e1", 10, "m").await.unwrap();
    assert!(changed(&mut thread), "a chief row was added");

    let reply_id = store
        .app_messages_since(0)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.sender == AppSender::User)
        .unwrap()
        .event_id;
    store
        .set_app_reply_state(&reply_id, AppReplyState::Typed)
        .await
        .unwrap();
    assert!(changed(&mut thread), "a reply was relabelled");

    store.delete_app_messages_before(i64::MAX).await.unwrap();
    assert!(changed(&mut thread), "rows were deleted");

    append_tts_mute_changed(&store, true).await.unwrap();
    store.project_unhandled_events(10).await.unwrap();
    assert!(
        !changed(&mut thread),
        "a projection that adds no row leaves the thread alone"
    );
}

#[tokio::test]
async fn an_existing_store_gains_002_and_keeps_its_data() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(directory.path()).await.unwrap();
    append_turn_completed(
        &store,
        &TurnCompleted {
            pane_id: "%1".into(),
            pane_label: "main:0.0".into(),
            last_user_prompt: "go".into(),
            assistant_message: "done".into(),
            main_context: String::new(),
            agent_incarnation: None,
            work_summary: CompletionSummaryUpdate::Unchanged,
        },
    )
    .await
    .unwrap();
    store.project_unhandled_events(10).await.unwrap();
    let turn_id = store
        .next_pending_delivery()
        .await
        .unwrap()
        .unwrap()
        .event_id;

    // Take the store back to what the base build left: no 002 record and no new tables.
    let conn = store.state.connect().unwrap();
    conn.execute_batch(
        "DELETE FROM _migrations WHERE name = '002_web_app';
         DROP TABLE app_messages;
         DROP TABLE app_push_subscriptions;",
    )
    .await
    .unwrap();
    assert!(!has_table(&conn, "app_messages").await);
    drop(conn);
    drop(store);

    let reopened = HaroldStore::open(directory.path()).await.unwrap();
    let pending = reopened.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(pending.event_id, turn_id);
    assert_eq!(pending.event_type, "TurnCompleted");
    let conn = reopened.state.connect().unwrap();
    assert!(has_table(&conn, "app_messages").await);
    assert!(has_table(&conn, "app_push_subscriptions").await);
    assert!(reopened.app_messages_since(0).await.unwrap().is_empty());
}
