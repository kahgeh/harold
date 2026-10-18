use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::backlog::v1::Item;
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

fn backlog_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tools.backlog.v1"))
        .expect("backlog package descriptor")
}

fn message_fields(name: &str) -> Vec<(String, i32)> {
    let descriptor = backlog_file_descriptor()
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
fn backlog_service_lists_its_eighteen_rpcs() {
    let descriptor = backlog_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("BacklogService"))
        .expect("BacklogService descriptor");
    let methods: Vec<String> = descriptor
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();

    assert_eq!(
        methods,
        [
            "ListProducts",
            "PutItem",
            "RankItems",
            "RemoveItem",
            "ListItems",
            "AddNote",
            "UpdateNote",
            "RemoveNote",
            "ListNotes",
            "AddQueueItem",
            "SetNow",
            "OrderQueue",
            "UpdateQueueItem",
            "DoneQueueItem",
            "SetQueueNote",
            "ListQueue",
            "Import",
            "Export",
        ]
    );
}

#[test]
fn item_note_and_queue_fields_keep_their_numbers() {
    assert_eq!(
        message_fields("Item"),
        fields(&[
            ("id", 1),
            ("title", 2),
            ("value", 3),
            ("size", 4),
            ("risk", 5),
            ("reason", 6),
            ("paths", 7),
            ("after", 8),
            ("evidence", 9),
            ("rank", 10),
            ("updated_at_ms", 11),
        ])
    );
    assert_eq!(
        message_fields("Note"),
        fields(&[
            ("id", 1),
            ("kind", 2),
            ("title", 3),
            ("body", 4),
            ("created_at_ms", 5),
            ("updated_at_ms", 6),
        ])
    );
    assert_eq!(
        message_fields("QueueItem"),
        fields(&[
            ("id", 1),
            ("state", 2),
            ("position", 3),
            ("text", 4),
            ("done_at_ms", 5),
            ("updated_at_ms", 6),
        ])
    );
    assert_eq!(
        message_fields("ImportCounts"),
        fields(&[
            ("items", 1),
            ("rule", 2),
            ("history", 3),
            ("held", 4),
            ("left_out", 5),
            ("now", 6),
            ("parked", 7),
            ("done", 8),
        ])
    );
    assert_eq!(
        message_fields("ImportRequest"),
        fields(&[
            ("product", 1),
            ("queue", 2),
            ("markdown", 3),
            ("apply", 4),
            ("caller_pane", 5),
            ("utc_offset_seconds", 6),
        ])
    );
}

#[test]
fn every_write_request_carries_caller_pane() {
    let writes = [
        "PutItem",
        "RankItems",
        "RemoveItem",
        "AddNote",
        "UpdateNote",
        "RemoveNote",
        "AddQueueItem",
        "SetNow",
        "OrderQueue",
        "UpdateQueueItem",
        "DoneQueueItem",
        "SetQueueNote",
        "Import",
    ];
    for write in writes {
        let name = format!("{write}Request");
        assert!(
            message_fields(&name)
                .iter()
                .any(|(field, _)| field == "caller_pane"),
            "{name} has no caller_pane"
        );
    }
    for read in [
        "ListItemsRequest",
        "ListNotesRequest",
        "ListQueueRequest",
        "ExportRequest",
        "ListProductsRequest",
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
fn an_item_round_trips_every_field() {
    let item = Item {
        id: "bq-1".to_string(),
        title: "title".to_string(),
        value: "value".to_string(),
        size: "small".to_string(),
        risk: "risk".to_string(),
        reason: "reason".to_string(),
        paths: "a/b".to_string(),
        after: "bq-0".to_string(),
        evidence: vec!["one".to_string(), "two".to_string()],
        rank: 3,
        updated_at_ms: 1_700_000_000_000,
    };
    assert_eq!(round_trip(&item), item);
}
