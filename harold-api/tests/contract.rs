use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::harold::{
    AgentPaneState, AgentState, ReportAgentStateRequest, TurnCompleteRequest,
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
        ]
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
        ]
    );
}
