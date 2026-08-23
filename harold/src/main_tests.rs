use std::sync::Arc;

use events::EventStreamVersion;

use super::harold::harold_server::Harold;
use super::harold::{AgentState, ReportAgentStateRequest, WatchAgentStatesRequest};
use super::{HaroldService, Request, TurnCompleteRequest, store};

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-service-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn turn_complete_accepts_only_after_appending_every_request_field() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let service = HaroldService {
        store: Arc::clone(&store),
    };
    let request = TurnCompleteRequest {
        pane_id: "%8".into(),
        pane_label: "harold:0.8".into(),
        last_user_prompt: "refresh events".into(),
        assistant_message: "events refreshed".into(),
        main_context: "harold".into(),
    };

    let response = service
        .turn_complete(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(response.accepted);

    let events = store
        .stream()
        .load_after_version(EventStreamVersion::start(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let event: store::TurnCompleted = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(event.pane_id, "%8");
    assert_eq!(event.pane_label, "harold:0.8");
    assert_eq!(event.last_user_prompt, "refresh events");
    assert_eq!(event.assistant_message, "events refreshed");
    assert_eq!(event.main_context, "harold");
}

#[tokio::test]
async fn agent_state_rpcs_are_explicitly_unimplemented_during_contract_stage() {
    let directory = TestDirectory::new();
    let store = Arc::new(store::HaroldStore::open(&directory.0).await.unwrap());
    let service = HaroldService { store };

    let report_error = service
        .report_agent_state(Request::new(ReportAgentStateRequest {
            pane_id: "%8".into(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".into(),
            work_summary: Some("refresh events".into()),
        }))
        .await
        .unwrap_err();
    assert_eq!(report_error.code(), tonic::Code::Unimplemented);

    let watch_error = service
        .watch_agent_states(Request::new(WatchAgentStatesRequest {}))
        .await
        .unwrap_err();
    assert_eq!(watch_error.code(), tonic::Code::Unimplemented);
}
