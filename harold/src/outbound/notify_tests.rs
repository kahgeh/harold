use std::time::Duration;

use super::fake::{FakeEnvironment, turn};
use super::{DeliveryOutcome, NotifyState, OutboundChannel, notify};
use crate::store::TurnCompleted;

fn notify_turn(
    environment: &FakeEnvironment,
    state: &NotifyState,
    turn: &TurnCompleted,
) -> Result<DeliveryOutcome, String> {
    notify(turn, "trace", state, environment)
}

#[test]
fn locked_screen_sends_the_update_to_the_phone_and_records_the_reply_target() {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();

    let outcome = notify_turn(&environment, &state, &turn("%1", "build it"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Away, "%1".to_string())]
    );
    assert_eq!(environment.reply_targets(), ["%1"]);
}

#[test]
fn unlocked_screen_speaks_the_update_and_records_no_reply_target() {
    let environment = FakeEnvironment::new("");
    let state = NotifyState::default();

    let outcome = notify_turn(&environment, &state, &turn("%1", "build it"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Tts, "%1".to_string())]
    );
    assert!(environment.reply_targets().is_empty());
}

#[test]
fn attached_session_is_skipped_only_while_the_screen_is_unlocked() {
    let environment = FakeEnvironment::new("");
    environment.set_session_attached(true);
    let state = NotifyState::default();

    let outcome = notify_turn(&environment, &state, &turn("%1", "build it"));
    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));
    assert!(environment.delivered().is_empty());

    environment.set_screen_locked(true);
    let outcome = notify_turn(&environment, &state, &turn("%1", "build it"));
    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
}

#[test]
fn active_pane_is_skipped_only_while_the_screen_is_unlocked() {
    let environment = FakeEnvironment::new(
        "[notify]\nskip_if_session_active = false\nskip_if_pane_active = true",
    );
    environment.set_active_pane(Some("%1"));
    let state = NotifyState::default();

    let outcome = notify_turn(&environment, &state, &turn("%1", "build it"));
    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));

    let outcome = notify_turn(&environment, &state, &turn("%2", "build it"));
    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));

    environment.set_screen_locked(true);
    let outcome = notify_turn(&environment, &state, &turn("%1", "build it"));
    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
}

#[test]
fn same_turn_within_30_seconds_is_sent_once() {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();
    let finished = turn("%1", "build it");

    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Delivered)
    );
    environment.advance(Duration::from_secs(29));
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    assert_eq!(environment.delivered().len(), 1);

    // 30 seconds after the first: outside the window.
    environment.advance(Duration::from_secs(1));
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Delivered)
    );
}

#[test]
fn another_prompt_or_pane_is_not_a_duplicate() {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();

    for finished in [
        turn("%1", "build it"),
        turn("%1", "test it"),
        turn("%2", "test it"),
    ] {
        assert_eq!(
            notify_turn(&environment, &state, &finished),
            Ok(DeliveryOutcome::Delivered)
        );
    }
    assert_eq!(environment.delivered().len(), 3);
}

#[test]
fn failed_delivery_is_not_recorded_so_the_retry_is_sent() {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();
    let finished = turn("%1", "build it");

    environment.set_fail_sends(true);
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Err("channel down".to_string())
    );

    environment.set_fail_sends(false);
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Delivered)
    );
}
