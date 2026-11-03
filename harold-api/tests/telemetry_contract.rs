use harold_api::FILE_DESCRIPTOR_SET;
use harold_api::tools::telemetry::v1::{
    Attr, CountRequest, DeclareRequest, DeclareResponse, EventRequest, GaugeRequest, Kind,
    ListRequest, ListResponse, Pair, RemoveRequest, Signal,
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
    #[prost(message, repeated, tag = "6")]
    service: Vec<ServiceDescriptorProto>,
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

fn pair(key: &str, value: &str) -> Pair {
    Pair {
        key: key.to_string(),
        value: value.to_string(),
    }
}

#[test]
fn every_request_and_response_round_trips() {
    let attr = Attr {
        key: "outcome".to_string(),
        values: vec!["ok".to_string(), "0-50".to_string()],
    };
    let declare = DeclareRequest {
        caller_pane: "pane-1".to_string(),
        name: "chief.merges".to_string(),
        kind: Kind::Counter as i32,
        meaning: "merges done".to_string(),
        attrs: vec![attr.clone()],
    };
    assert_eq!(round_trip(&declare), declare);
    assert_eq!(round_trip(&DeclareResponse {}), DeclareResponse {});

    let count = CountRequest {
        caller_pane: "pane-1".to_string(),
        name: "chief.merges".to_string(),
        n: 3,
        attrs: vec![pair("outcome", "ok")],
    };
    assert_eq!(round_trip(&count), count);

    let gauge = GaugeRequest {
        caller_pane: "pane-1".to_string(),
        name: "chief.depth".to_string(),
        value: 2.5,
        attrs: vec![pair("lane", "a")],
    };
    assert_eq!(round_trip(&gauge), gauge);

    let event = EventRequest {
        caller_pane: "pane-1".to_string(),
        name: "chief.started".to_string(),
        attrs: vec![pair("step", "1")],
    };
    assert_eq!(round_trip(&event), event);

    let list = ListRequest {
        caller_pane: "pane-1".to_string(),
        prefix: "chief".to_string(),
    };
    assert_eq!(round_trip(&list), list);

    let reply = ListResponse {
        signals: vec![Signal {
            name: "chief.merges".to_string(),
            kind: Kind::Event as i32,
            meaning: "merges done".to_string(),
            attrs: vec![attr],
        }],
    };
    assert_eq!(round_trip(&reply), reply);

    let remove = RemoveRequest {
        caller_pane: "pane-1".to_string(),
        name: "chief.merges".to_string(),
    };
    assert_eq!(round_trip(&remove), remove);
}

#[test]
fn telemetry_service_lists_its_six_rpcs() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("decode descriptor set")
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("harold.tools.telemetry.v1"))
        .expect("telemetry package descriptor")
        .service
        .into_iter()
        .find(|service| service.name.as_deref() == Some("TelemetryService"))
        .expect("TelemetryService descriptor");
    let methods: Vec<String> = descriptor
        .method
        .into_iter()
        .map(|method| method.name.expect("method name"))
        .collect();

    assert_eq!(
        methods,
        ["Declare", "Count", "Gauge", "Event", "List", "Remove"]
    );
}
