use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::questions::v1::{
    AnswerKind, AnswerQuestionRequest, ListQuestionsResponse, ListedQuestion, Pick, SpecState,
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
    #[prost(message, repeated, tag = "5")]
    enum_type: Vec<EnumDescriptorProto>,
    #[prost(message, repeated, tag = "6")]
    service: Vec<ServiceDescriptorProto>,
}

#[derive(Clone, PartialEq, Message)]
struct DescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(message, repeated, tag = "2")]
    field: Vec<FieldDescriptorProto>,
    #[prost(message, repeated, tag = "9")]
    reserved_range: Vec<ReservedRange>,
    #[prost(string, repeated, tag = "10")]
    reserved_name: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ReservedRange {
    #[prost(int32, optional, tag = "1")]
    start: Option<i32>,
    /// Exclusive.
    #[prost(int32, optional, tag = "2")]
    end: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct FieldDescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(int32, optional, tag = "3")]
    number: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumDescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(message, repeated, tag = "2")]
    value: Vec<EnumValueDescriptorProto>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumValueDescriptorProto {
    #[prost(string, optional, tag = "1")]
    name: Option<String>,
    #[prost(int32, optional, tag = "2")]
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

fn questions_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tools.questions.v1"))
        .expect("questions package descriptor")
}

fn message_descriptor(name: &str) -> DescriptorProto {
    questions_file_descriptor()
        .message_type
        .into_iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {name} descriptor"))
}

fn message_fields(name: &str) -> Vec<(String, i32)> {
    let descriptor = questions_file_descriptor()
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

fn fields(expected: &[(&str, i32)]) -> Vec<(String, i32)> {
    expected
        .iter()
        .map(|(name, number)| (name.to_string(), *number))
        .collect()
}

#[test]
fn question_service_lists_its_ten_rpcs() {
    let descriptor = questions_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("QuestionService"))
        .expect("QuestionService descriptor");
    let methods: Vec<String> = descriptor
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();

    assert_eq!(
        methods,
        [
            "PutQuestion",
            "DismissQuestion",
            "ReorderQuestions",
            "ListQuestions",
            "AnswerQuestion",
            "TickStep",
            "SavePicks",
            "ShowQuestion",
            "MarkChecked",
        ]
    );
}

#[test]
fn question_fields_keep_their_numbers() {
    assert_eq!(
        message_fields("DecisionOption"),
        fields(&[
            ("key", 1),
            ("label", 2),
            ("impact", 3),
            ("suggested", 4),
            ("free", 5)
        ])
    );
    assert_eq!(
        message_fields("Decision"),
        fields(&[("ask", 1), ("options", 2)])
    );
    assert_eq!(
        message_fields("Question"),
        fields(&[
            ("id", 1),
            ("product", 2),
            ("spec", 3),
            ("sha", 4),
            ("link", 5),
            ("intro", 6),
            ("decisions", 7),
            ("title", 8),
            ("steps", 9),
            ("spec_file", 10),
        ])
    );
    assert_eq!(
        message_fields("Step"),
        fields(&[("text", 1), ("detail", 2)])
    );
    assert_eq!(
        message_fields("PutQuestionRequest"),
        fields(&[("question", 1)])
    );
    assert_eq!(
        message_fields("PutQuestionResponse"),
        fields(&[("id", 1), ("version", 2)])
    );
    assert_eq!(
        message_fields("DismissQuestionRequest"),
        fields(&[("id", 1), ("reason", 2)])
    );
    assert!(message_fields("DismissQuestionResponse").is_empty());
    assert_eq!(
        message_fields("ReorderQuestionsRequest"),
        fields(&[("ids", 1)])
    );
    assert!(message_fields("ReorderQuestionsResponse").is_empty());
    assert!(message_fields("ListQuestionsRequest").is_empty());
    assert_eq!(
        message_fields("ListQuestionsResponse"),
        fields(&[
            ("questions", 1),
            ("waiting", 2),
            ("dismissed", 3),
            ("checked_at_ms", 4)
        ])
    );
    assert_eq!(
        message_fields("ListedQuestion"),
        fields(&[
            ("question", 1),
            ("version", 2),
            ("ticked", 4),
            ("picks", 5),
            ("spec_state", 6)
        ])
    );
    assert_eq!(
        message_fields("Dismissed"),
        fields(&[("id", 1), ("reason", 2)])
    );
    assert_eq!(
        message_fields("Pick"),
        fields(&[("decision", 1), ("key", 2), ("text", 3)])
    );
    assert_eq!(
        message_fields("AnswerQuestionRequest"),
        fields(&[
            ("id", 1),
            ("version", 2),
            ("kind", 3),
            ("picks", 4),
            ("note", 5),
            ("send_now", 6),
            ("trace_parent", 7)
        ])
    );
    assert!(message_fields("AnswerQuestionResponse").is_empty());
    for gone in ["DeferQuestionRequest", "DeferQuestionResponse"] {
        assert!(
            !questions_file_descriptor()
                .message_type
                .iter()
                .any(|message| message.name.as_deref() == Some(gone)),
            "{gone} is removed"
        );
    }
    assert_eq!(
        message_fields("TickStepRequest"),
        fields(&[("id", 1), ("version", 2), ("step", 3), ("ticked", 4)])
    );
    assert!(message_fields("TickStepResponse").is_empty());
    assert_eq!(
        message_fields("SavePicksRequest"),
        fields(&[("id", 1), ("version", 2), ("picks", 3)])
    );
    assert!(message_fields("SavePicksResponse").is_empty());
    assert_eq!(message_fields("ShowQuestionRequest"), fields(&[("id", 1)]));
    assert_eq!(
        message_fields("ShowQuestionResponse"),
        fields(&[
            ("question", 1),
            ("version", 2),
            ("state", 3),
            ("reason", 4),
            ("picks", 5),
            ("spec_state", 6)
        ])
    );
    assert!(message_fields("MarkCheckedRequest").is_empty());
    assert_eq!(
        message_fields("MarkCheckedResponse"),
        fields(&[("checked_at_ms", 1)])
    );
}

#[test]
fn spec_states_keep_their_numbers() {
    let descriptor = questions_file_descriptor()
        .enum_type
        .into_iter()
        .find(|value| value.name.as_deref() == Some("SpecState"))
        .expect("SpecState descriptor");
    let values: Vec<(String, i32)> = descriptor
        .value
        .into_iter()
        .map(|value| (value.name.expect("name"), value.number.expect("number")))
        .collect();
    assert_eq!(
        values,
        fields(&[
            ("SPEC_STATE_UNKNOWN", 0),
            ("SPEC_STATE_CURRENT", 1),
            ("SPEC_STATE_CHANGED", 2),
        ])
    );
}

#[test]
fn a_listed_question_from_an_older_server_has_an_unknown_spec_state_and_no_check() {
    // ListedQuestion with only field 2 (version) = 1, as a server before the spec state sends it.
    let old = ListedQuestion::decode([0x10, 0x01].as_slice()).expect("decode");
    assert_eq!(old.spec_state, SpecState::Unknown as i32);
    let none = ListQuestionsResponse::decode([0x10, 0x00].as_slice()).expect("decode");
    assert_eq!(none.checked_at_ms, 0);
}

#[test]
fn answer_kinds_keep_their_numbers() {
    let descriptor = questions_file_descriptor()
        .enum_type
        .into_iter()
        .find(|value| value.name.as_deref() == Some("AnswerKind"))
        .expect("AnswerKind descriptor");
    let values: Vec<(String, i32)> = descriptor
        .value
        .into_iter()
        .map(|value| (value.name.expect("name"), value.number.expect("number")))
        .collect();
    assert_eq!(
        values,
        fields(&[
            ("ANSWER_KIND_UNSPECIFIED", 0),
            ("ANSWER_KIND_ANSWER", 1),
            ("ANSWER_KIND_REVISE", 2),
            ("ANSWER_KIND_REJECT", 3),
            ("ANSWER_KIND_FREE_TEXT", 4),
            ("ANSWER_KIND_DONE", 5),
            ("ANSWER_KIND_IGNORE", 6),
            ("ANSWER_KIND_RELEGATE", 7),
        ])
    );
    assert_eq!(AnswerKind::Answer as i32, 1);
    assert_eq!(AnswerKind::FreeText as i32, 4);
    assert_eq!(AnswerKind::Relegate as i32, 7);
}

#[test]
fn listed_question_reserves_the_removed_deferred_field() {
    let listed = message_descriptor("ListedQuestion");
    let ranges: Vec<(i32, i32)> = listed
        .reserved_range
        .iter()
        .map(|range| (range.start.expect("start"), range.end.expect("end")))
        .collect();
    assert_eq!(ranges, [(3, 4)]);
    assert_eq!(listed.reserved_name, ["deferred"]);
}

#[test]
fn answer_question_request_round_trips_with_picks() {
    let request = AnswerQuestionRequest {
        id: "q-1".to_string(),
        version: 3,
        kind: AnswerKind::FreeText as i32,
        picks: vec![
            Pick {
                decision: 0,
                key: "a".to_string(),
                text: String::new(),
            },
            Pick {
                decision: 1,
                key: "other".to_string(),
                text: "something else".to_string(),
            },
        ],
        note: "n".to_string(),
        send_now: false,
        trace_parent: String::new(),
    };
    assert_eq!(round_trip(&request), request);
}

#[test]
fn answer_question_request_omitting_send_now_decodes_unmarked() {
    // Field 1 (id) = "q-1" only, as an older client sends it.
    let bytes = [0x0a, 0x03, b'q', b'-', b'1'];
    let request = AnswerQuestionRequest::decode(bytes.as_slice()).expect("decode");
    assert!(!request.send_now);

    let marked = AnswerQuestionRequest {
        send_now: true,
        ..request
    };
    let decoded = AnswerQuestionRequest::decode(marked.encode_to_vec().as_slice()).expect("decode");
    assert!(decoded.send_now);
}

#[test]
fn answer_question_request_carries_a_trace_parent_and_older_bytes_have_none() {
    let request = AnswerQuestionRequest {
        id: "q-1".to_string(),
        trace_parent: "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01".to_string(),
        ..Default::default()
    };
    assert_eq!(round_trip(&request), request);

    // Field 1 (id) = "q-1" only, as an older client sends it.
    let bytes = [0x0a, 0x03, b'q', b'-', b'1'];
    let old = AnswerQuestionRequest::decode(bytes.as_slice()).expect("decode");
    assert!(old.trace_parent.is_empty());
}
