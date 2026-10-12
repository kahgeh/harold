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

// ---------------------------------------------------------------------------
// Rule A: no update after a message the pane asked Harold to send
// ---------------------------------------------------------------------------

/// A locked-screen environment and a state in which pane %1 has just sent a message.
fn marked_pane() -> (FakeEnvironment, NotifyState) {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();
    state.mark_requested_message("%1", super::NotifyEnvironment::now(&environment));
    (environment, state)
}

#[test]
fn update_after_a_requested_message_is_left_out() {
    let (environment, state) = marked_pane();
    environment.advance(Duration::from_secs(5));

    let outcome = notify_turn(&environment, &state, &turn("%1", "report in"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
}

#[test]
fn mark_affects_one_turn_only() {
    let (environment, state) = marked_pane();

    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Skipped)
    );
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "carry on")),
        Ok(DeliveryOutcome::Delivered)
    );
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Away, "%1".to_string())]
    );
}

#[test]
fn mark_older_than_the_limit_has_no_effect() {
    let (environment, state) = marked_pane();
    environment.advance(Duration::from_secs(600));
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Delivered)
    );

    let (environment, state) = marked_pane();
    environment.advance(Duration::from_secs(599));
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Skipped)
    );
}

#[test]
fn later_requested_message_replaces_the_mark() {
    let (environment, state) = marked_pane();
    environment.advance(Duration::from_secs(540));
    state.mark_requested_message("%1", super::NotifyEnvironment::now(&environment));
    environment.advance(Duration::from_secs(300));

    // 14 minutes after the first message, 5 after the second.
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Skipped)
    );
}

#[test]
fn mark_does_not_affect_another_pane_and_survives_its_turn() {
    let (environment, state) = marked_pane();

    assert_eq!(
        notify_turn(&environment, &state, &turn("%2", "build it")),
        Ok(DeliveryOutcome::Delivered)
    );
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Skipped)
    );
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Away, "%2".to_string())]
    );
}

#[test]
fn turn_ending_at_the_desk_is_spoken_and_removes_the_mark() {
    let (environment, state) = marked_pane();
    environment.set_screen_locked(false);

    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Delivered)
    );
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Tts, "%1".to_string())]
    );

    environment.set_screen_locked(true);
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "carry on")),
        Ok(DeliveryOutcome::Delivered)
    );
}

#[test]
fn turn_skipped_at_the_desk_still_removes_the_mark() {
    let (environment, state) = marked_pane();
    environment.set_screen_locked(false);
    environment.set_session_attached(true);

    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "report in")),
        Ok(DeliveryOutcome::Skipped)
    );

    environment.set_screen_locked(true);
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "carry on")),
        Ok(DeliveryOutcome::Delivered)
    );
}

#[test]
fn repeat_of_a_left_out_turn_is_left_out_as_a_duplicate() {
    let (environment, state) = marked_pane();
    let finished = turn("%1", "report in");

    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    // The mark is gone now: only the duplicate check can stop the repeat.
    environment.advance(Duration::from_secs(10));
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    assert!(environment.delivered().is_empty());
}

#[test]
fn paused_update_is_skipped_as_before_and_removes_the_mark() {
    let (environment, state) = marked_pane();
    let finished = turn("%1", "report in");

    crate::outbound::set_messaging_paused(true);
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    crate::outbound::set_messaging_paused(false);

    // The paused skip is not recorded as a duplicate and the mark is gone, so the same
    // turn is sent when it comes again.
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Delivered)
    );
}
