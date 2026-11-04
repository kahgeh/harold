use super::*;
use crate::outbound::NotifyState;
use crate::outbound::fake::{FakeEnvironment, turn};
use crate::outbound::left_out::TrackedAgents;
use crate::outbound::notify;
use crate::settings::settings_with;
use crate::store::AwayMessageRequested;

const PAIRED: &str = "[master]\naddress = \"mac.tailnet:4333\"";

fn question() -> AwayMessageRequested {
    AwayMessageRequested {
        pane_id: "%1".into(),
        pane_label: "label".into(),
        text: "which branch?".into(),
    }
}

#[test]
fn only_a_harold_with_a_master_table_defers_delivery() {
    assert!(delivers_locally(&settings_with("")));
    assert!(!delivers_locally(&settings_with(PAIRED)));
}

#[test]
fn paired_slave_sends_no_phone_push_speech_or_todo_for_a_turn_end() {
    // A locked screen is the case that reaches the phone on a stand-alone Harold.
    let gate = DeliveryGate::new(
        FakeEnvironment::locked(""),
        delivers_locally(&settings_with(PAIRED)),
    );
    let outcome = notify(
        &turn("%1", "build it"),
        "trace",
        &NotifyState::default(),
        &TrackedAgents::default(),
        &gate,
    );
    assert!(outcome.is_ok());
    assert!(gate.inner.delivered().is_empty());
    assert!(gate.inner.spoken_lines().is_empty());
    assert!(gate.inner.away_messages().is_empty());
    assert!(gate.inner.reply_targets().is_empty());
}

#[test]
fn paired_slave_sends_no_phone_push_speech_or_todo_for_a_question() {
    let gate = DeliveryGate::new(
        FakeEnvironment::locked(""),
        delivers_locally(&settings_with(PAIRED)),
    );
    assert_eq!(gate.send_away_message(&question()), Ok(()));
    assert_eq!(gate.speak_line("which branch?"), Ok(()));
    assert!(gate.inner.away_messages().is_empty());
    assert!(gate.inner.spoken_lines().is_empty());
    assert!(!gate.channel_set_up(Channel::Imessage));
    assert!(!gate.channel_set_up(Channel::WebApp));
}

#[test]
fn unpaired_linux_harold_keeps_stand_alone_delivery() {
    let gate = DeliveryGate::new(
        FakeEnvironment::locked(""),
        delivers_locally(&settings_with("")),
    );
    assert_eq!(gate.send_away_message(&question()), Ok(()));
    assert_eq!(gate.speak_line("which branch?"), Ok(()));
    assert_eq!(gate.inner.away_messages(), ["which branch?"]);
    assert_eq!(gate.inner.spoken_lines(), ["which branch?"]);
    assert!(gate.channel_set_up(Channel::Imessage));
    #[cfg(feature = "imessage")]
    {
        let outcome = notify(
            &turn("%1", "build it"),
            "trace",
            &NotifyState::default(),
            &TrackedAgents::default(),
            &gate,
        );
        assert!(outcome.is_ok());
        assert_eq!(gate.inner.delivered().len(), 1);
    }
}

#[test]
fn a_master_table_without_a_secret_keeps_delivery_silent() {
    // No `master.secret` exists in this test: the table alone decides, and the safe side is silent.
    assert!(!delivers_locally(&settings_with(PAIRED)));
}
