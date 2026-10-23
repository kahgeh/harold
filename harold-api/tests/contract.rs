use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::harold::{
    AgentPaneState, AgentState, AgentStateSnapshot, CallAction, CallRequest, CallResponse,
    CallResult, CallState, ReportAgentStateRequest, SendMessageToAwayDeviceRequest,
    SendMessageToAwayDeviceResponse, SetMessagingPausedRequest, SetMessagingPausedResponse,
    SetTtsMutedRequest, SetTtsMutedResponse, SpeakLineRequest, SpeakLineResponse,
    TurnCompleteRequest,
};
use prost::Message;

#[derive(Clone, PartialEq, Message)]
struct FileDescriptorSet {
    #[prost(message, repeated, tag = "1")]
    file: Vec<FileDescriptorProto>,
}

#[derive(Clone, PartialEq, Message)]
struct FileDescriptorProto {
    #[prost(string, optional, tag = "2")]
    package: Option<String>,
    #[prost(message, repeated, tag = "4")]
    message_type: Vec<DescriptorProto>,
    #[prost(message, repeated, tag = "6")]
    service: Vec<ServiceDescriptorProto>,
}

#[derive(Clone, PartialEq, Message)]
struct DescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(message, repeated, tag = "2")]
    field: Vec<FieldDescriptorProto>,
}

#[derive(Clone, PartialEq, Message)]
struct FieldDescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(int32, optional, tag = "3")]
    number: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct ServiceDescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(message, repeated, tag = "2")]
    method: Vec<MethodDescriptorProto>,
}

#[derive(Clone, PartialEq, Message)]
struct MethodDescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
}

fn round_trip<T>(value: &T) -> T
where
    T: Message + Default,
{
    T::decode(value.encode_to_vec().as_slice()).expect("decode round-trip message")
}

fn harold_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode Harold descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold"))
        .expect("Harold package descriptor")
}

fn message_fields(name: &str) -> Vec<(String, i32)> {
    let descriptor = harold_file_descriptor()
        .message_type
        .into_iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {name} descriptor"));

    descriptor
        .field
        .into_iter()
        .map(|field| {
            (
                field.name.expect("field name"),
                field.number.expect("field number"),
            )
        })
        .collect()
}

#[test]
fn report_agent_state_preserves_optional_summary_presence() {
    for work_summary in [None, Some(String::new()), Some("index events".to_string())] {
        let request = ReportAgentStateRequest {
            pane_id: "%7".to_string(),
            state: AgentState::Busy.into(),
            adapter_id: "codex-hook".to_string(),
            work_summary: work_summary.clone(),
        };

        let decoded = round_trip(&request);

        assert_eq!(decoded.work_summary, work_summary);
    }

    assert_eq!(
        message_fields("ReportAgentStateRequest"),
        [
            ("pane_id".to_string(), 1),
            ("state".to_string(), 2),
            ("adapter_id".to_string(), 3),
            ("work_summary".to_string(), 4),
        ]
    );
}

#[test]
fn pane_summary_round_trip_preserves_presence_without_provenance() {
    for work_summary in [
        None,
        Some(String::new()),
        Some("review projector".to_string()),
    ] {
        let pane = AgentPaneState {
            pane_id: "%9".to_string(),
            state: AgentState::Idle.into(),
            work_summary: work_summary.clone(),
            ..Default::default()
        };

        let decoded = round_trip(&pane);

        assert_eq!(decoded.work_summary, work_summary);
    }

    assert_eq!(
        message_fields("AgentPaneState"),
        [
            ("pane_id".to_string(), 1),
            ("tmux_target".to_string(), 2),
            ("session_name".to_string(), 3),
            ("window_index".to_string(), 4),
            ("pane_index".to_string(), 5),
            ("pane_pid".to_string(), 6),
            ("agent_pid".to_string(), 7),
            ("agent_started_at_ms".to_string(), 8),
            ("provider_id".to_string(), 9),
            ("provider_display_name".to_string(), 10),
            ("working_directory".to_string(), 11),
            ("state".to_string(), 12),
            ("last_transition_at_ms".to_string(), 13),
            ("work_summary".to_string(), 14),
        ]
    );
}

#[test]
fn turn_complete_preserves_legacy_wire_fields() {
    let request = TurnCompleteRequest {
        pane_id: "%8".to_string(),
        pane_label: "harold:0.8".to_string(),
        last_user_prompt: "refresh events".to_string(),
        assistant_message: "events refreshed".to_string(),
        main_context: "harold".to_string(),
        ..Default::default()
    };

    let decoded = round_trip(&request);

    assert_eq!(decoded.pane_id, "%8");
    assert_eq!(decoded.pane_label, "harold:0.8");
    assert_eq!(decoded.last_user_prompt, "refresh events");
    assert_eq!(decoded.assistant_message, "events refreshed");
    assert_eq!(decoded.main_context, "harold");
    assert_eq!(
        message_fields("TurnCompleteRequest"),
        [
            ("pane_id".to_string(), 1),
            ("pane_label".to_string(), 2),
            ("last_user_prompt".to_string(), 3),
            ("assistant_message".to_string(), 4),
            ("main_context".to_string(), 5),
            ("session_id".to_string(), 6),
            ("turn_started_at".to_string(), 7),
            ("transcript_path".to_string(), 8),
        ]
    );
}

#[test]
fn turn_complete_request_carries_session_and_start() {
    let request = TurnCompleteRequest {
        session_id: "abc".into(),
        turn_started_at: "2026-10-15T08:00:00Z".into(),
        transcript_path: "/t/s.jsonl".into(),
        ..Default::default()
    };
    let bytes = request.encode_to_vec();
    let decoded = TurnCompleteRequest::decode(&bytes[..]).unwrap();
    assert_eq!(
        (
            decoded.session_id.as_str(),
            decoded.turn_started_at.as_str(),
            decoded.transcript_path.as_str()
        ),
        ("abc", "2026-10-15T08:00:00Z", "/t/s.jsonl")
    );
    // An old hook sends neither field.
    let old_hook = TurnCompleteRequest {
        pane_id: "%8".into(),
        ..Default::default()
    };
    let decoded = TurnCompleteRequest::decode(&old_hook.encode_to_vec()[..]).unwrap();
    assert_eq!(
        (
            decoded.session_id.as_str(),
            decoded.turn_started_at.as_str(),
            decoded.transcript_path.as_str()
        ),
        ("", "", "")
    );
}

#[test]
fn harold_service_exposes_only_the_approved_rpcs() {
    let descriptor = harold_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("Harold"))
        .expect("Harold service descriptor");
    let methods: Vec<String> = descriptor
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();

    assert_eq!(
        methods,
        [
            "TurnComplete".to_string(),
            "ReportAgentState".to_string(),
            "WatchAgentStates".to_string(),
            "SetTtsMuted".to_string(),
            "SetMessagingPaused".to_string(),
            "SendMessageToAwayDevice".to_string(),
            "SpeakLine".to_string(),
            "Call".to_string(),
        ]
    );
}

#[test]
fn set_tts_muted_contract_is_additive() {
    let request = SetTtsMutedRequest { muted: true };
    assert!(round_trip(&request).muted);
    assert!(round_trip(&SetTtsMutedResponse { muted: true }).muted);
    assert_eq!(
        message_fields("SetTtsMutedRequest"),
        [("muted".to_string(), 1)]
    );
    let snapshot_fields = message_fields("AgentStateSnapshot");
    assert!(snapshot_fields.contains(&("tts_muted".to_string(), 5)));
    assert_eq!(snapshot_fields.len(), 7);
}

#[test]
fn set_messaging_paused_contract_is_additive() {
    let request = SetMessagingPausedRequest { paused: true };
    assert!(round_trip(&request).paused);
    assert!(round_trip(&SetMessagingPausedResponse { paused: true }).paused);
    assert_eq!(
        message_fields("SetMessagingPausedRequest"),
        [("paused".to_string(), 1)]
    );
    assert_eq!(
        message_fields("SetMessagingPausedResponse"),
        [("paused".to_string(), 1)]
    );
    let snapshot_fields = message_fields("AgentStateSnapshot");
    assert!(snapshot_fields.contains(&("messaging_paused".to_string(), 6)));
    assert_eq!(snapshot_fields.len(), 7);
}

#[test]
fn send_message_to_away_device_carries_the_source_pane_and_the_text() {
    let request = SendMessageToAwayDeviceRequest {
        src_id: "%47".to_string(),
        text: "Pod 3 is blocked".to_string(),
    };
    assert_eq!(round_trip(&request), request);
    assert!(round_trip(&SendMessageToAwayDeviceResponse { accepted: true }).accepted);
    assert_eq!(
        message_fields("SendMessageToAwayDeviceRequest"),
        [("src_id".to_string(), 1), ("text".to_string(), 2)]
    );
    assert_eq!(
        message_fields("SendMessageToAwayDeviceResponse"),
        [("accepted".to_string(), 1)]
    );
}

#[test]
fn speak_line_carries_the_source_pane_and_the_text() {
    let request = SpeakLineRequest {
        src_id: "%47".to_string(),
        text: "Release install is on task five of six.".to_string(),
    };
    assert_eq!(round_trip(&request), request);
    assert_eq!(
        message_fields("SpeakLineRequest"),
        [("src_id".to_string(), 1), ("text".to_string(), 2)]
    );
    assert!(message_fields("SpeakLineResponse").is_empty());
    let _ = round_trip(&SpeakLineResponse {});
}

#[test]
fn call_request_and_response_round_trip() {
    let request = CallRequest {
        action: CallAction::TalkNow as i32,
    };
    assert_eq!(round_trip(&request), request);
    assert_eq!(message_fields("CallRequest"), [("action".to_string(), 1)]);

    for result in [
        CallResult::Unspecified,
        CallResult::Typed,
        CallResult::NothingHeard,
        CallResult::NoChief,
        CallResult::ChiefBusy,
        CallResult::Failed,
        CallResult::VoiceMuted,
    ] {
        let response = CallResponse {
            state: CallState::Speaking as i32,
            chief_pane_id: "%47".to_string(),
            last_transcript: "hello chief".to_string(),
            last_result: result as i32,
        };
        assert_eq!(round_trip(&response), response);
    }
    assert_eq!(CallResult::VoiceMuted as i32, 6);
    assert_eq!(CallState::NotSetUp as i32, 6);
    assert_eq!(CallAction::Status as i32, 4);
}

#[test]
fn service_lists_call() {
    let methods: Vec<String> = harold_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("Harold"))
        .expect("Harold service descriptor")
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();
    assert!(methods.contains(&"Call".to_string()));
    assert_eq!(
        message_fields("CallResponse"),
        [
            ("state".to_string(), 1),
            ("chief_pane_id".to_string(), 2),
            ("last_transcript".to_string(), 3),
            ("last_result".to_string(), 4),
        ]
    );
}

#[test]
fn agent_state_snapshot_carries_harold_version_in_field_seven() {
    let snapshot = AgentStateSnapshot {
        harold_version: "v0.9.0".into(),
        ..Default::default()
    };
    assert_eq!(round_trip(&snapshot).harold_version, "v0.9.0");
    let fields = message_fields("AgentStateSnapshot");
    for expected in [
        ("through_event_version", 1),
        ("server_time_ms", 2),
        ("monitor_health", 3),
        ("panes", 4),
        ("tts_muted", 5),
        ("messaging_paused", 6),
        ("harold_version", 7),
    ] {
        assert!(
            fields.contains(&(expected.0.to_string(), expected.1)),
            "{expected:?}"
        );
    }
    assert_eq!(fields.len(), 7);
}
