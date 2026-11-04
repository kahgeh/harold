use harold_api::harold::AgentStateSnapshot;
use harold_api::harold::slave::v1::{
    Ack, Hello, Lock, MasterMessage, SlaveMessage, Snapshot, Turn, Welcome, master_message,
    slave_message,
};
use prost::Message;

fn round_trip_slave(message: SlaveMessage) {
    let decoded = SlaveMessage::decode(message.encode_to_vec().as_slice()).expect("decode slave");
    assert_eq!(decoded, message);
}

fn round_trip_master(message: MasterMessage) {
    let decoded = MasterMessage::decode(message.encode_to_vec().as_slice()).expect("decode master");
    assert_eq!(decoded, message);
}

#[test]
fn slave_link_messages_round_trip() {
    round_trip_slave(SlaveMessage {
        body: Some(slave_message::Body::Hello(Hello {
            name: "omarchy".into(),
            version: "0.9.0".into(),
            last_known_report: 7,
            protocol: 1,
        })),
    });
    round_trip_slave(SlaveMessage {
        body: Some(slave_message::Body::Snapshot(Snapshot {
            snapshot: Some(AgentStateSnapshot {
                through_event_version: 3,
                ..Default::default()
            }),
            backlog: 2,
        })),
    });
    let turn = Turn {
        report_no: 8,
        pane_id: "block:12".into(),
        pane_label: "work".into(),
        session_id: "s1".into(),
        turn_started_at: "2026-11-05T01:02:03Z".into(),
        last_user_prompt: "p".into(),
        assistant_message: "a".into(),
        main_context: "main".into(),
        input_tokens: 1,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        model: "m".into(),
        transcript_path: String::new(),
    };
    round_trip_slave(SlaveMessage {
        body: Some(slave_message::Body::Turn(turn)),
    });
    round_trip_slave(SlaveMessage {
        body: Some(slave_message::Body::Lock(Lock {
            report_no: 9,
            locked: true,
        })),
    });
    round_trip_master(MasterMessage {
        body: Some(master_message::Body::Welcome(Welcome { last_report: 7 })),
    });
    round_trip_master(MasterMessage {
        body: Some(master_message::Body::Ack(Ack { report_no: 9 })),
    });
    assert_eq!(Turn::default().transcript_path, "");
}
