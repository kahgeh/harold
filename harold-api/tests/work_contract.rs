use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::work::v1::{Decision, WorkRecord};
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

fn work_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tools.work.v1"))
        .expect("work package descriptor")
}

fn message_fields(name: &str) -> Vec<(String, i32)> {
    work_file_descriptor()
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

fn fields(expected: &[(&str, i32)]) -> Vec<(String, i32)> {
    expected
        .iter()
        .map(|(name, number)| (name.to_string(), *number))
        .collect()
}

#[test]
fn work_service_lists_its_thirteen_rpcs() {
    let methods: Vec<String> = work_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("WorkService"))
        .expect("WorkService descriptor")
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();

    assert_eq!(
        methods,
        [
            "List", "Show", "Put", "Set", "Decide", "Mark", "Link", "Unlink", "Check", "Import",
            "Rotate", "History", "Search",
        ]
    );
}

#[test]
fn record_decision_and_mismatch_fields_keep_their_numbers() {
    assert_eq!(
        message_fields("WorkRecord"),
        fields(&[
            ("name", 1),
            ("title", 2),
            ("priority", 3),
            ("status", 4),
            ("waits_for", 5),
            ("next_step", 6),
            ("created_in", 7),
            ("created_at_ms", 8),
            ("updated_at_ms", 9),
            ("status_since_ms", 10),
            ("decisions", 11),
            ("links", 12),
        ])
    );
    assert_eq!(
        message_fields("Decision"),
        fields(&[("at", 1), ("who", 2), ("mark", 3), ("text", 4)])
    );
    assert_eq!(
        message_fields("Mismatch"),
        fields(&[("record", 1), ("kind", 2), ("line", 3)])
    );
    assert_eq!(
        message_fields("SetRequest"),
        fields(&[
            ("name", 1),
            ("title", 2),
            ("priority", 3),
            ("status", 4),
            ("waits_for", 5),
            ("next_step", 6),
            ("caller_pane", 7),
        ])
    );
    assert_eq!(
        message_fields("ImportRequest"),
        fields(&[
            ("folder", 1),
            ("queue", 2),
            ("apply", 3),
            ("caller_pane", 4)
        ])
    );
    assert_eq!(
        message_fields("RotateResponse"),
        fields(&[
            ("open_records", 1),
            ("decisions", 2),
            ("links", 3),
            ("records", 4),
            ("target", 5),
        ])
    );
}

#[test]
fn request_and_response_fields_keep_their_numbers() {
    let expected: [(&str, &[(&str, i32)]); 8] = [
        ("PutRequest", &[("record", 1), ("caller_pane", 2)]),
        (
            "DecideRequest",
            &[
                ("name", 1),
                ("mark", 2),
                ("who", 3),
                ("text", 4),
                ("caller_pane", 5),
            ],
        ),
        (
            "MarkRequest",
            &[("name", 1), ("number", 2), ("mark", 3), ("caller_pane", 4)],
        ),
        (
            "LinkRequest",
            &[("name", 1), ("link", 2), ("caller_pane", 3)],
        ),
        (
            "UnlinkRequest",
            &[("name", 1), ("link", 2), ("caller_pane", 3)],
        ),
        ("ListResponse", &[("records", 1), ("file", 2)]),
        ("HistoryResponse", &[("file", 1), ("record", 2)]),
        ("Found", &[("file", 1), ("record", 2)]),
    ];
    for (message, numbers) in expected {
        assert_eq!(message_fields(message), fields(numbers), "{message}");
    }
}

#[test]
fn every_write_request_carries_caller_pane() {
    for write in ["Put", "Set", "Decide", "Mark", "Link", "Unlink", "Import"] {
        let name = format!("{write}Request");
        assert!(
            message_fields(&name)
                .iter()
                .any(|(field, _)| field == "caller_pane"),
            "{name} has no caller_pane"
        );
    }
    for read in [
        "ListRequest",
        "ShowRequest",
        "CheckRequest",
        "RotateRequest",
        "HistoryRequest",
        "SearchRequest",
    ] {
        assert!(
            !message_fields(read)
                .iter()
                .any(|(field, _)| field == "caller_pane"),
            "{read} has a caller_pane"
        );
    }
}

#[test]
fn a_record_round_trips_every_field() {
    let record = WorkRecord {
        name: "work-records".into(),
        title: "Work records".into(),
        priority: "focus".into(),
        status: "building".into(),
        waits_for: "the user".into(),
        next_step: "merge".into(),
        created_in: "work-2026-11-03.db".into(),
        created_at_ms: 1,
        updated_at_ms: 2,
        status_since_ms: 3,
        decisions: vec![Decision {
            at: "2026-11-03".into(),
            who: "the chief".into(),
            mark: "agreed".into(),
            text: "one".into(),
        }],
        links: vec!["spec:harold/work-records".into()],
    };
    assert_eq!(
        WorkRecord::decode(record.encode_to_vec().as_slice()).unwrap(),
        record
    );
}
