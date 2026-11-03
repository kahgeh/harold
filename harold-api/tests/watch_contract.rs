use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::watch::v1::{
    PauseRequest, RemindRequest, StartWatchRequest, Watch, remind_request,
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
    #[prost(bool, optional, tag = "17")]
    proto3_optional: Option<bool>,
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

fn watch_file_descriptor() -> FileDescriptorProto {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tools.watch.v1"))
        .expect("watch package descriptor")
}

fn message_fields(name: &str) -> Vec<FieldDescriptorProto> {
    watch_file_descriptor()
        .message_type
        .into_iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {name} descriptor"))
        .field
}

fn field(message: &str, name: &str) -> (i32, bool) {
    let found = message_fields(message)
        .into_iter()
        .find(|field| field.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {message}.{name}"));
    (
        found.number.expect("field number"),
        found.proto3_optional.unwrap_or(false),
    )
}

#[test]
fn watch_service_lists_its_six_rpcs() {
    let methods: Vec<String> = watch_file_descriptor()
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("WatchService"))
        .expect("WatchService descriptor")
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();
    assert_eq!(
        methods,
        [
            "StartWatch",
            "StopWatch",
            "ListWatches",
            "Pause",
            "Resume",
            "Remind"
        ]
    );
}

#[test]
fn the_three_end_fields_are_optional_and_keep_their_numbers() {
    assert_eq!(field("StartWatchRequest", "for_minutes"), (5, true));
    assert_eq!(field("StartWatchResponse", "ends_at_ms"), (2, true));
    assert_eq!(field("Watch", "ends_at_ms"), (7, true));
    let request = StartWatchRequest {
        for_minutes: None,
        ..Default::default()
    };
    assert_eq!(request.for_minutes, None);
    assert_eq!(Watch::default().ends_at_ms, None);
}

#[test]
fn the_new_fields_have_the_numbers_the_spec_gives() {
    assert_eq!(field("Watch", "paused"), (15, false));
    assert_eq!(field("Watch", "paused_until_ms"), (16, true));
    assert_eq!(field("Watch", "pause_how"), (17, false));
    assert_eq!(field("ListWatchesResponse", "reminders"), (3, false));
    assert_eq!(field("PauseRequest", "for_minutes"), (3, true));
    assert_eq!(field("PauseRequest", "caller"), (4, false));
    assert_eq!(field("ResumeRequest", "caller"), (3, false));
    assert_eq!(field("RemindRequest", "in_minutes"), (3, false));
    assert_eq!(field("RemindRequest", "at_local"), (4, false));
    assert_eq!(field("RemindRequest", "report_to"), (5, false));
    assert_eq!(field("RemindRequest", "text"), (6, false));
    assert_eq!(field("RemindResponse", "due_at_ms"), (1, false));
    assert_eq!(field("PauseResponse", "skipped"), (2, false));
    assert_eq!(field("ResumeResponse", "skipped"), (2, false));
    assert_eq!(field("Reminder", "due_at_ms"), (3, false));
    assert_eq!(PauseRequest::default().for_minutes, None);
}

#[test]
fn the_cron_fields_have_new_numbers_and_the_old_ones_stay() {
    // `every_minutes` keeps number 4 and becomes optional.
    assert_eq!(field("StartWatchRequest", "every_minutes"), (4, true));
    assert_eq!(field("StartWatchRequest", "cron"), (10, true));
    assert_eq!(field("RemindRequest", "in_minutes"), (3, false));
    assert_eq!(field("RemindRequest", "at_local"), (4, false));
    assert_eq!(field("RemindRequest", "cron"), (7, false));
    assert_eq!(field("Watch", "cron"), (18, false));
    assert_eq!(field("Watch", "checked_local"), (19, false));
    assert_eq!(field("Reminder", "cron"), (4, false));

    let request = StartWatchRequest::default();
    assert_eq!((request.every_minutes, request.cron), (None, None));
    let remind = RemindRequest {
        when: Some(remind_request::When::Cron("0 9 * * *".into())),
        ..Default::default()
    };
    assert!(matches!(remind.when, Some(remind_request::When::Cron(_))));
}
