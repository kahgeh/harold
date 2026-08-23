use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use events::EventStreamVersion;

use crate::agent::domain::{
    AgentEvent, AgentIncarnation, AgentLifecycleObserved, AgentMonitorHealthChanged,
    AgentPaneDeparted, AgentPaneObservation, AgentPaneObserved, AgentScreenObserved,
    CompletionSummaryUpdate, EffectiveAgentState, ObservedAgentState, WorkSummaryUpdate,
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
        .load_after_version(EventStreamVersion::start(), 10)
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
        agent_incarnation: None,
        work_summary: CompletionSummaryUpdate::Unchanged,
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
async fn projection_migration_is_idempotent_and_preserves_earlier_records() {
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
    assert_eq!(
        names,
        [
            "001_last_processed_event",
            "002_delivery_outbox",
            "003_agent_monitor_projection",
        ]
    );

    conn.execute(
        "UPDATE _migrations SET checksum = 'changed' WHERE name = '003_agent_monitor_projection'",
        (),
    )
    .await
    .unwrap();
    drop(conn);
    drop(store);

    let error = HaroldStore::open(directory.path())
        .await
        .err()
        .expect("changed projection migration checksum should fail");
    assert!(error.to_string().contains("checksum changed"));
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
    let pending = reopened.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(pending.event_type, "InboundMessageReceived");
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
        .load_after_version(EventStreamVersion::start(), 500)
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
    let store = HaroldStore::open_with_hook_grace(directory.path(), 0)
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

    let reopened = HaroldStore::open_with_hook_grace(directory.path(), 0)
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
    assert_eq!(snapshot.through_event_version, EventStreamVersion::start());
}
