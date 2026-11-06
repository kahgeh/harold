use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::harold::{
    AgentPaneState, AgentSeenRequest, AgentSeenResponse, AgentState, AgentStateSnapshot,
    CallAction, CallRequest, CallResponse, CallResult, CallState, GetHaroldStateResponse,
    ReportAgentStateRequest, SendMessageToAwayDeviceRequest, SendMessageToAwayDeviceResponse,
    SetMessagingPausedRequest, SetMessagingPausedResponse, SetTtsMutedRequest, SetTtsMutedResponse,
    SpeakLineRequest, SpeakLineResponse, TurnCompleteRequest,
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
            state: AgentState::Working.into(),
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

    for recap in [None, Some(String::new()), Some("plan done".to_string())] {
        for recap_at_ms in [0, 1_234] {
            let pane = AgentPaneState {
                pane_id: "%9".to_string(),
                recap: recap.clone(),
                recap_at_ms,
                ..Default::default()
            };

            let decoded = round_trip(&pane);

            assert_eq!(decoded.recap, recap);
            assert_eq!(decoded.recap_at_ms, recap_at_ms);
        }
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
            ("recap".to_string(), 15),
            ("recap_at_ms".to_string(), 16),
            ("current_step".to_string(), 17),
            ("host".to_string(), 18),
            ("host_icon".to_string(), 19),
            ("window_name".to_string(), 20),
            ("role".to_string(), 21),
            ("pod_product".to_string(), 22),
            ("waiting".to_string(), 23),
            ("waiting_since_ms".to_string(), 24),
        ]
    );

    let remote = AgentPaneState {
        pane_id: "rex:omarchy/block:abc".to_string(),
        host: "omarchy".to_string(),
        host_icon: "arch".to_string(),
        window_name: "linux-satellite-2b".to_string(),
        ..Default::default()
    };
    let decoded = round_trip(&remote);
    assert_eq!(decoded.host, "omarchy");
    assert_eq!(decoded.host_icon, "arch");
    assert_eq!(decoded.window_name, "linux-satellite-2b");
    assert!(round_trip(&AgentPaneState::default()).host.is_empty());
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
            ("input_tokens".to_string(), 9),
            ("output_tokens".to_string(), 10),
            ("cache_read_tokens".to_string(), 11),
            ("cache_write_tokens".to_string(), 12),
            ("model".to_string(), 13),
        ]
    );
}

#[test]
fn turn_complete_request_carries_turn_tokens() {
    let request = TurnCompleteRequest {
        input_tokens: 11,
        output_tokens: 22,
        cache_read_tokens: 33,
        cache_write_tokens: 44,
        model: "claude-test-1".to_string(),
        ..Default::default()
    };

    let decoded = round_trip(&request);

    assert_eq!(decoded.input_tokens, 11);
    assert_eq!(decoded.output_tokens, 22);
    assert_eq!(decoded.cache_read_tokens, 33);
    assert_eq!(decoded.cache_write_tokens, 44);
    assert_eq!(decoded.model, "claude-test-1");
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
fn proto_keeps_the_old_busy_name_as_an_alias() {
    // An installed opencode plugin sends AGENT_STATE_BUSY through grpcurl,
    // which reads the names from this file.
    let proto = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/proto/harold.proto"))
        .expect("read harold.proto");
    assert!(proto.contains("AGENT_STATE_BUSY = 1"));
    assert!(proto.contains("allow_alias = true"));
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
            "GetHaroldState".to_string(),
            "GetRemoteOrder".to_string(),
            "SetRemoteOrder".to_string(),
            "AgentSeen".to_string(),
        ]
    );
}

#[test]
fn agent_pane_state_has_role_21_and_pod_product_22() {
    let fields = message_fields("AgentPaneState");
    assert!(fields.contains(&("role".to_string(), 21)));
    assert!(fields.contains(&("pod_product".to_string(), 22)));
}

#[test]
fn harold_service_lists_the_remote_order_calls() {
    let methods: Vec<String> = harold_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("Harold"))
        .expect("Harold service descriptor")
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();
    assert!(methods.contains(&"GetRemoteOrder".to_string()));
    assert!(methods.contains(&"SetRemoteOrder".to_string()));
    assert_eq!(
        message_fields("GetRemoteOrderResponse"),
        [("products".to_string(), 1)]
    );
    assert_eq!(
        message_fields("SetRemoteOrderRequest"),
        [("products".to_string(), 1)]
    );
}

#[test]
fn get_harold_state_fields_are_pinned() {
    let expected: Vec<(String, i32)> = [
        ("tts_muted", 1),
        ("messaging_paused", 2),
        ("release_version", 3),
        ("machine_name", 4),
        ("multiplexer", 5),
        ("rex_hosts", 6),
        ("machine_host_icon", 7),
        ("tts_engine", 8),
        ("chief_session", 9),
        ("call_open", 10),
        ("call_where", 11),
        ("call_opened_at_ms", 12),
    ]
    .into_iter()
    .map(|(name, number)| (name.to_string(), number))
    .collect();
    assert_eq!(message_fields("GetHaroldStateResponse"), expected);
}

#[test]
fn get_harold_state_call_open_is_written_when_false() {
    let response = GetHaroldStateResponse {
        call_open: Some(false),
        ..Default::default()
    };
    assert_eq!(response.encode_to_vec(), vec![0x50, 0x00]);
    let open = GetHaroldStateResponse {
        call_open: Some(true),
        call_where: "phone".to_string(),
        call_opened_at_ms: 1_762_000_000_000,
        ..Default::default()
    };
    let decoded = round_trip(&open);
    assert_eq!(decoded.call_open, Some(true));
    assert_eq!(decoded.call_where, "phone");
    assert_eq!(decoded.call_opened_at_ms, 1_762_000_000_000);
}

#[test]
fn rex_host_icon_fields_are_pinned() {
    assert_eq!(
        message_fields("RexHostIcon"),
        [("label".to_string(), 1), ("host_icon".to_string(), 2)]
    );
}

#[test]
fn get_harold_state_flags_are_written_when_false() {
    let response = GetHaroldStateResponse {
        tts_muted: Some(false),
        messaging_paused: Some(false),
        ..Default::default()
    };
    let bytes = response.encode_to_vec();
    assert_eq!(&bytes[..4], &[0x08, 0x00, 0x10, 0x00]);
    let decoded = round_trip(&response);
    assert_eq!(decoded.tts_muted, Some(false));
    assert_eq!(decoded.messaging_paused, Some(false));
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
    assert_eq!(snapshot_fields.len(), 11);
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
    assert_eq!(snapshot_fields.len(), 11);
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
        ("watches_running", 8),
        ("watches_paused", 9),
        ("watches_capped", 10),
        ("focus_known", 11),
    ] {
        assert!(
            fields.contains(&(expected.0.to_string(), expected.1)),
            "{expected:?}"
        );
    }
    assert_eq!(fields.len(), 11);
}

#[test]
fn watch_counts_contract_is_additive() {
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
    let snapshot = AgentStateSnapshot {
        watches_running: 2,
        watches_paused: 1,
        watches_capped: 3,
        ..Default::default()
    };
    let decoded = round_trip(&snapshot);
    assert_eq!(decoded.watches_running, 2);
    assert_eq!(decoded.watches_paused, 1);
    assert_eq!(decoded.watches_capped, 3);
}

#[test]
fn knowledge_graph_messages_round_trip_and_keep_their_field_numbers() {
    use harold_api::tools::knowledge::v1::{
        GraphRelation, GraphStatusResponse, RebuildGraphResponse, RelateRequest,
        RelationsOfResponse, VoidGraphRecordRequest,
    };

    let relate = RelateRequest {
        from: "a".into(),
        relation: "part_of".into(),
        to: "b".into(),
    };
    assert_eq!(round_trip(&relate), relate);
    let relations = RelationsOfResponse {
        relations: vec![GraphRelation {
            name: "b".into(),
            title: "B".into(),
            relation: "links".into(),
            outgoing: true,
            missing: true,
        }],
    };
    assert_eq!(round_trip(&relations), relations);
    let status = GraphStatusResponse {
        enabled: true,
        has_data_position: true,
        data_position: 1285,
        waiting: 2,
        has_last_refusal: true,
        last_refusal_sequence: 1291,
        last_refusal_reason: "refused".into(),
        ..Default::default()
    };
    assert_eq!(round_trip(&status), status);
    let rebuilt = RebuildGraphResponse {
        schema_steps: 1,
        records: 1284,
        ..Default::default()
    };
    assert_eq!(round_trip(&rebuilt), rebuilt);
    let void = VoidGraphRecordRequest {
        sequence: 1291,
        reason: "typo".into(),
    };
    assert_eq!(round_trip(&void), void);
}

fn tts_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode Harold descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tts.v1"))
        .expect("TTS package descriptor")
}

fn tts_message_fields(name: &str) -> Vec<(String, i32)> {
    tts_file_descriptor()
        .message_type
        .into_iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {name} descriptor"))
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
fn tts_service_lists_its_five_rpcs() {
    let service = tts_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("TtsService"))
        .expect("TtsService descriptor");
    let methods: Vec<String> = service
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();

    assert_eq!(
        methods,
        [
            "Speak",
            "ListModels",
            "AddModel",
            "RemoveModel",
            "SetDefault"
        ]
    );
}

#[test]
fn tts_fields_keep_their_numbers() {
    let named = |fields: &[(&str, i32)]| -> Vec<(String, i32)> {
        fields
            .iter()
            .map(|(name, number)| (name.to_string(), *number))
            .collect()
    };

    assert_eq!(
        tts_message_fields("SpeakRequest"),
        named(&[("text", 1), ("model", 2), ("voice", 3)])
    );
    assert_eq!(
        tts_message_fields("AudioChunk"),
        named(&[("sample_rate", 1), ("pcm", 2)])
    );
    assert_eq!(
        tts_message_fields("Model"),
        named(&[
            ("id", 1),
            ("source", 2),
            ("revision", 3),
            ("worker", 4),
            ("voices", 5),
            ("state", 6),
            ("reason", 7),
            ("size_bytes", 8),
            ("is_default", 9),
            ("default_voice", 10),
        ])
    );
    assert_eq!(
        tts_message_fields("AddModelRequest"),
        named(&[("source", 1), ("repo", 2), ("revision", 3)])
    );
    assert_eq!(
        tts_message_fields("SetDefaultRequest"),
        named(&[("model", 1), ("voice", 2)])
    );
}

#[test]
fn a_tts_audio_chunk_round_trips() {
    use harold_api::tts::v1::{AudioChunk, Model, ModelState};

    let chunk = AudioChunk {
        sample_rate: 24_000,
        pcm: vec![1, 0, 255, 127],
    };
    assert_eq!(round_trip(&chunk), chunk);

    let model = Model {
        id: "mlx-community/Kokoro-82M-bf16".to_string(),
        source: "huggingface".to_string(),
        revision: "a".repeat(40),
        worker: "mlx".to_string(),
        voices: vec!["af_heart".to_string()],
        state: ModelState::Failed.into(),
        reason: "check_failed".to_string(),
        size_bytes: 389_410_807,
        is_default: true,
        default_voice: "af_heart".to_string(),
    };
    assert_eq!(round_trip(&model), model);
}

#[test]
fn agent_state_has_working_idle_unknown_on_numbers_1_2_3() {
    assert_eq!(AgentState::Working as i32, 1);
    assert_eq!(AgentState::Idle as i32, 2);
    assert_eq!(AgentState::Unknown as i32, 3);
    assert_eq!(AgentState::try_from(1), Ok(AgentState::Working));
    assert_eq!(
        AgentState::from_str_name("AGENT_STATE_WORKING"),
        Some(AgentState::Working)
    );
}

#[test]
fn agent_state_numbers_are_stable() {
    assert_eq!(
        [
            AgentState::Unspecified,
            AgentState::Working,
            AgentState::Idle,
            AgentState::Unknown,
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Error,
        ]
        .map(i32::from),
        [0, 1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        AgentState::from_str_name("AGENT_STATE_BLOCKED"),
        Some(AgentState::Blocked)
    );
    assert_eq!(
        AgentState::from_str_name("AGENT_STATE_DONE"),
        Some(AgentState::Done)
    );
    assert_eq!(
        AgentState::from_str_name("AGENT_STATE_ERROR"),
        Some(AgentState::Error)
    );
}

#[test]
fn waiting_fields_have_numbers_23_and_24_and_focus_known_is_11() {
    let pane_fields = message_fields("AgentPaneState");
    assert!(pane_fields.contains(&("waiting".to_string(), 23)));
    assert!(pane_fields.contains(&("waiting_since_ms".to_string(), 24)));
    let snapshot_fields = message_fields("AgentStateSnapshot");
    assert!(snapshot_fields.contains(&("focus_known".to_string(), 11)));
}

/// Golden bytes of a waiting pane: field 23 (tag b801) is true, field 24 (tag c001) is 1234.
#[test]
fn a_waiting_pane_has_the_golden_bytes() {
    let pane = AgentPaneState {
        waiting: true,
        waiting_since_ms: 1234,
        ..Default::default()
    };
    assert_eq!(
        pane.encode_to_vec(),
        [0xb8, 0x01, 0x01, 0xc0, 0x01, 0xd2, 0x09]
    );
    let decoded = AgentPaneState::decode([0xb8, 0x01, 0x01, 0xc0, 0x01, 0xd2, 0x09].as_slice())
        .expect("a pane");
    assert!(decoded.waiting);
    assert_eq!(decoded.waiting_since_ms, 1234);
}

/// Golden bytes of a snapshot with `focus_known`: field 11 (tag 58). Absent stays `None`.
#[test]
fn focus_known_has_the_golden_bytes_and_absent_is_none() {
    for (value, bytes) in [(true, [0x58, 0x01]), (false, [0x58, 0x00])] {
        let snapshot = AgentStateSnapshot {
            focus_known: Some(value),
            ..Default::default()
        };
        assert_eq!(snapshot.encode_to_vec(), bytes);
        assert_eq!(round_trip(&snapshot).focus_known, Some(value));
    }
    assert_eq!(
        AgentStateSnapshot::default().encode_to_vec(),
        Vec::<u8>::new()
    );
    assert_eq!(round_trip(&AgentStateSnapshot::default()).focus_known, None);
}

/// An old Harold sends none of the new fields. The old golden row still decodes, and the
/// new fields read as false and zero.
#[test]
fn the_old_golden_row_decodes_with_no_waiting() {
    let hex = include_str!("fixtures/host_agent_row.hex").trim();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("a hex pair"))
        .collect();
    let pane = AgentPaneState::decode(bytes.as_slice()).expect("the old row");
    assert!(!pane.waiting);
    assert_eq!(pane.waiting_since_ms, 0);
}

/// A control-tower tells Harold it jumped to a pane. The request has only the pane id,
/// field 1, and the answer is empty.
#[test]
fn agent_seen_has_pane_id_1_and_an_empty_response() {
    let methods: Vec<String> = harold_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("Harold"))
        .expect("Harold service descriptor")
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();
    assert!(methods.contains(&"AgentSeen".to_string()));
    assert_eq!(
        message_fields("AgentSeenRequest"),
        [("pane_id".to_string(), 1)]
    );
    assert_eq!(
        message_fields("AgentSeenResponse"),
        Vec::<(String, i32)>::new()
    );
    let request = AgentSeenRequest {
        pane_id: "%7".to_owned(),
    };
    assert_eq!(request.encode_to_vec(), [0x0a, 0x02, b'%', b'7']);
    assert_eq!(round_trip(&request).pane_id, "%7");
    assert_eq!(AgentSeenResponse {}.encode_to_vec(), Vec::<u8>::new());
}
