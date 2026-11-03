use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::release::v1::{Line, NextResponse, ProductNext};
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

fn release_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tools.release.v1"))
        .expect("release package descriptor")
}

fn message_fields(name: &str) -> Vec<(String, i32)> {
    release_file_descriptor()
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
fn release_service_lists_its_one_rpc() {
    let descriptor = release_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("ReleaseService"))
        .expect("ReleaseService descriptor");
    let methods: Vec<String> = descriptor
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();
    assert_eq!(methods, ["Next"]);
}

#[test]
fn messages_keep_their_field_numbers() {
    assert_eq!(message_fields("NextRequest"), fields(&[("product", 1)]));
    assert_eq!(message_fields("NextResponse"), fields(&[("products", 1)]));
    assert_eq!(
        message_fields("ProductNext"),
        fields(&[
            ("name", 1),
            ("paused", 2),
            ("release", 3),
            ("merged", 4),
            ("in_progress", 5),
            ("approved", 6),
            ("parked", 7),
            ("awaiting", 8),
            ("not_clear", 9),
            ("other_commits", 10),
            ("unavailable", 11),
        ])
    );
    assert_eq!(
        message_fields("Line"),
        fields(&[
            ("id", 1),
            ("title", 2),
            ("commits", 3),
            ("commits_known", 4)
        ])
    );
}

#[test]
fn a_response_round_trips_every_field() {
    let line = Line {
        id: "a-1".to_string(),
        title: "title".to_string(),
        commits: 2,
        commits_known: true,
    };
    let response = NextResponse {
        products: vec![ProductNext {
            name: "harold".to_string(),
            paused: true,
            release: "v1.0.0".to_string(),
            merged: vec![line.clone()],
            in_progress: vec![line.clone()],
            approved: vec![line.clone()],
            parked: vec![line.clone()],
            awaiting: vec![line.clone()],
            not_clear: vec![line],
            other_commits: 3,
            unavailable: "reason".to_string(),
        }],
    };
    assert_eq!(round_trip(&response), response);
}
