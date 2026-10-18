use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::questions::v1::{AnswerKind, AnswerQuestionRequest, Pick};
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
fn question_service_lists_its_six_rpcs() {
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
            "DeferQuestion",
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
        ])
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
        fields(&[("questions", 1), ("waiting", 2), ("dismissed", 3)])
    );
    assert_eq!(
        message_fields("ListedQuestion"),
        fields(&[("question", 1), ("version", 2), ("deferred", 3)])
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
            ("note", 5)
        ])
    );
    assert!(message_fields("AnswerQuestionResponse").is_empty());
    assert_eq!(message_fields("DeferQuestionRequest"), fields(&[("id", 1)]));
    assert!(message_fields("DeferQuestionResponse").is_empty());
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
        ])
    );
    assert_eq!(AnswerKind::Answer as i32, 1);
    assert_eq!(AnswerKind::FreeText as i32, 4);
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
    };
    assert_eq!(round_trip(&request), request);
}
