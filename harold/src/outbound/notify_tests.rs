use std::sync::Mutex;
use std::time::Duration;

use super::NotifyEnvironment;
use super::fake::{CapturedLogs, FakeEnvironment, turn};
use super::left_out::TrackedAgents;
use super::{
    AppDelivery, AppRoute, AwayText, ChiefApp, DeliveryOutcome, NotifyState, OutboundChannel,
    is_chief_pane, notify, notify_with_app,
};
use crate::inbound::ownership::agent_message_text;
use crate::store::{AppReplyState, TurnCompleted};

fn notify_turn(
    environment: &FakeEnvironment,
    state: &NotifyState,
    turn: &TurnCompleted,
) -> Result<DeliveryOutcome, String> {
    notify_turn_among(environment, state, &TrackedAgents::default(), turn)
}

fn notify_turn_among(
    environment: &FakeEnvironment,
    state: &NotifyState,
    agents: &TrackedAgents,
    turn: &TurnCompleted,
) -> Result<DeliveryOutcome, String> {
    notify(turn, "trace", state, agents, environment)
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
fn duplicate_turn_still_removes_the_mark() {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();
    let finished = turn("%1", "report in");
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Delivered)
    );

    // The pane is marked, then the turn already sent comes again inside 30 seconds. The
    // duplicate check stops it, and the mark must be gone all the same: it is removed
    // before any other check.
    state.mark_requested_message("%1", super::NotifyEnvironment::now(&environment));
    environment.advance(Duration::from_secs(10));
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Skipped)
    );

    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "carry on")),
        Ok(DeliveryOutcome::Delivered)
    );
    assert_eq!(environment.delivered().len(), 2);
}

#[test]
fn failed_delivery_keeps_the_mark_for_the_retry_of_the_same_turn() {
    let (environment, state) = marked_pane();
    let finished = turn("%1", "report in");

    // The turn ends at the desk and the spoken update fails.
    environment.set_screen_locked(false);
    environment.set_fail_sends(true);
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Err("channel down".to_string())
    );

    // The screen is locked before the retry: the pane messaged the phone this turn, so
    // the retry is left out.
    environment.set_screen_locked(true);
    environment.set_fail_sends(false);
    environment.advance(Duration::from_secs(5));
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    assert!(environment.delivered().is_empty());

    // The retry removed the mark: the pane's next turn sends its update.
    assert_eq!(
        notify_turn(&environment, &state, &turn("%1", "carry on")),
        Ok(DeliveryOutcome::Delivered)
    );
}

#[test]
fn mark_put_back_after_a_failed_delivery_keeps_the_time_it_was_set() {
    let (environment, state) = marked_pane();
    let finished = turn("%1", "report in");

    environment.set_screen_locked(false);
    environment.set_fail_sends(true);
    environment.advance(Duration::from_secs(590));
    assert!(notify_turn(&environment, &state, &finished).is_err());

    // 600 seconds after the message: the limit runs from the message, not from the retry.
    environment.set_screen_locked(true);
    environment.set_fail_sends(false);
    environment.advance(Duration::from_secs(10));
    assert_eq!(
        notify_turn(&environment, &state, &finished),
        Ok(DeliveryOutcome::Delivered)
    );
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

// ---------------------------------------------------------------------------
// The channel lists
// ---------------------------------------------------------------------------

/// The chief's folder is the only one on the phone's list (iMessage) and the desk's.
const ONLY_FROM_CHIEF: &str = "[notify]\nchief = \"/work/chief-of-pods\"\nimessage = [\"/work/chief-of-pods\"]\nmachine_audio_output = [\"/work/chief-of-pods\"]";

/// Pane %1 runs an agent in the chief's folder, pane %2 runs one in another folder.
fn chief_and_pod() -> TrackedAgents {
    TrackedAgents::in_folders(&[("%1", "/work/chief-of-pods"), ("%2", "/work/harold")])
}

#[test]
fn a_turn_is_the_chiefs_when_its_pane_is_the_one_notify_chief_finds() {
    let environment = FakeEnvironment::new(ONLY_FROM_CHIEF);
    let settings = environment.notify_settings();
    assert!(is_chief_pane(settings, &chief_and_pod(), "%1"));
    assert!(!is_chief_pane(settings, &chief_and_pod(), "%2"));
    assert!(!is_chief_pane(settings, &chief_and_pod(), "%9"));
}

#[test]
fn pane_off_the_list_sends_nothing_to_the_phone() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();
    let finished = turn("%2", "build it");

    let outcome = notify_turn_among(&environment, &state, &chief_and_pod(), &finished);

    assert_eq!(
        outcome,
        Ok(DeliveryOutcome::HeldBack {
            reason: "not_listed",
        })
    );
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());

    // Recorded in the duplicate check: the repeat is left out whatever the panes are.
    let listed = TrackedAgents::in_folders(&[("%2", "/work/chief-of-pods")]);
    assert_eq!(
        notify_turn_among(&environment, &state, &listed, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    assert!(environment.delivered().is_empty());
}

#[test]
fn pane_on_the_list_sends_its_update() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();

    let outcome = notify_turn_among(
        &environment,
        &state,
        &chief_and_pod(),
        &turn("%1", "report"),
    );

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert_eq!(environment.reply_targets(), ["%1"]);
}

#[test]
fn with_no_agent_in_a_listed_folder_every_pane_is_held_back_on_both_channels() {
    let pod_only = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
    for environment in [
        FakeEnvironment::locked(ONLY_FROM_CHIEF),
        FakeEnvironment::new(ONLY_FROM_CHIEF),
    ] {
        let outcome = notify_turn_among(
            &environment,
            &NotifyState::default(),
            &pod_only,
            &turn("%2", "build it"),
        );

        assert_eq!(
            outcome,
            Ok(DeliveryOutcome::HeldBack {
                reason: "not_listed",
            })
        );
        assert!(environment.delivered().is_empty());
    }
}

#[test]
fn the_default_lists_speak_nobody_but_a_chief_of_pods_folder() {
    // The shipped defaults, not FakeEnvironment's open lists: lists named, all as shipped.
    let defaults = "[notify]\nimessage = [\"chief-of-pods\"]\ntelegram = [\"chief-of-pods\"]\nweb_app = [\"chief-of-pods\"]\nmachine_audio_output = [\"chief-of-pods\"]";
    let agents =
        TrackedAgents::in_folders(&[("%1", "/any/where/chief-of-pods"), ("%2", "/work/harold")]);
    let environment = FakeEnvironment::locked(defaults);

    for pane in ["%1", "%2"] {
        notify_turn_among(
            &environment,
            &NotifyState::default(),
            &agents,
            &turn(pane, "x"),
        )
        .unwrap();
    }

    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Away, "%1".to_string())]
    );
}

#[test]
fn desk_speaks_only_listed_agents_and_never_the_chief() {
    let overrides = "[notify]\nchief = \"/work/chief-of-pods\"\nmachine_audio_output = [\"/work/chief-of-pods\", \"/work/listed\"]";
    let environment = FakeEnvironment::new(overrides);
    let agents = TrackedAgents::in_folders(&[
        ("%1", "/work/chief-of-pods"),
        ("%2", "/work/listed"),
        ("%3", "/work/harold-worktree"),
        ("%4", "/work/harold"),
        ("%5", "/work/architect"),
        ("%6", "/work/listed/sub"),
    ]);
    let logs = CapturedLogs::start();

    for pane in ["%1", "%2", "%3", "%4", "%5", "%6", "%9"] {
        notify_turn_among(
            &environment,
            &NotifyState::default(),
            &agents,
            &turn(pane, "build it"),
        )
        .unwrap();
    }

    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Tts, "%2".to_string())]
    );
    assert_eq!(environment.delivered_as_chief(), [false]);
    let text = logs.text();
    assert!(
        text.contains("spoken update left out (chief) pane_id=%1"),
        "{text}"
    );
    for pane in ["%3", "%4", "%5", "%6", "%9"] {
        assert!(
            text.contains(&format!(
                "spoken update left out (not in notify.machine_audio_output) pane_id={pane}"
            )),
            "{pane}: {text}"
        );
    }
}

#[test]
fn an_empty_machine_audio_output_silences_the_desk_but_not_the_phone() {
    let agents = chief_and_pod();
    let desk = FakeEnvironment::new("[notify]\nmachine_audio_output = []");
    notify_turn_among(&desk, &NotifyState::default(), &agents, &turn("%2", "x")).unwrap();
    assert!(desk.delivered().is_empty());

    let phone = FakeEnvironment::locked("[notify]\nmachine_audio_output = []");
    notify_turn_among(&phone, &NotifyState::default(), &agents, &turn("%2", "x")).unwrap();
    assert_eq!(
        phone.delivered(),
        [(OutboundChannel::Away, "%2".to_string())]
    );
}

#[test]
fn a_star_list_speaks_every_pane_even_one_with_no_folder() {
    let environment = FakeEnvironment::new("[notify]\nmachine_audio_output = [\"*\"]");

    notify_turn_among(
        &environment,
        &NotifyState::default(),
        &TrackedAgents::default(),
        &turn("%9", "x"),
    )
    .unwrap();

    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Tts, "%9".to_string())]
    );
}

#[test]
fn an_empty_phone_list_holds_every_pane_back_naming_its_key() {
    let environment = FakeEnvironment::locked("[notify]\nimessage = []");
    let logs = CapturedLogs::start();

    let outcome = notify_turn_among(
        &environment,
        &NotifyState::default(),
        &chief_and_pod(),
        &turn("%1", "x"),
    );

    assert_eq!(
        outcome,
        Ok(DeliveryOutcome::HeldBack {
            reason: "not_listed",
        })
    );
    assert!(
        logs.text()
            .contains("notification skipped (not in notify.imessage) pane_id=%1")
    );
}

#[test]
fn telegram_list_decides_when_telegram_is_the_away_channel() {
    let environment = FakeEnvironment::locked(
        "[notify]\naway_channel = \"telegram\"\nimessage = [\"/work/harold\"]\ntelegram = [\"/work/chief-of-pods\"]",
    );

    for pane in ["%1", "%2"] {
        notify_turn_among(
            &environment,
            &NotifyState::default(),
            &chief_and_pod(),
            &turn(pane, "x"),
        )
        .unwrap();
    }

    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Away, "%1".to_string())]
    );
}

#[test]
fn agent_on_the_list_that_messaged_this_turn_gets_no_update() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();
    state.mark_requested_message("%1", super::NotifyEnvironment::now(&environment));

    let outcome = notify_turn_among(
        &environment,
        &state,
        &chief_and_pod(),
        &turn("%1", "report"),
    );

    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));
    assert!(environment.delivered().is_empty());
}

#[test]
fn the_voice_mute_and_the_messaging_pause_come_before_the_lists() {
    let agents = chief_and_pod();
    super::set_tts_muted(true);
    let muted = FakeEnvironment::new(ONLY_FROM_CHIEF);
    let logs = CapturedLogs::start();
    notify_turn_among(&muted, &NotifyState::default(), &agents, &turn("%2", "x")).unwrap();
    super::set_tts_muted(false);
    assert!(
        logs.text()
            .contains("TTS notification skipped (voice muted)")
    );

    super::set_messaging_paused(true);
    let paused = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let logs = CapturedLogs::start();
    notify_turn_among(&paused, &NotifyState::default(), &agents, &turn("%2", "x")).unwrap();
    super::set_messaging_paused(false);
    assert!(
        logs.text()
            .contains("away notification skipped (messaging paused)")
    );
}

#[test]
fn update_left_out_after_a_requested_message_is_logged_with_its_pane() {
    let (environment, state) = marked_pane();
    let logs = CapturedLogs::start();

    let outcome = notify_turn(&environment, &state, &turn("%1", "report in"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));
    let text = logs.text();
    assert!(
        text.contains("notification skipped (pane messaged the phone this turn) pane_id=%1"),
        "{text}"
    );
}

#[test]
fn update_left_out_by_a_list_is_logged_with_its_pane_and_key() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();
    let logs = CapturedLogs::start();

    let outcome = notify_turn_among(
        &environment,
        &state,
        &chief_and_pod(),
        &turn("%2", "build it"),
    );

    assert_eq!(
        outcome,
        Ok(DeliveryOutcome::HeldBack {
            reason: "not_listed",
        })
    );
    let text = logs.text();
    assert!(
        text.contains("notification skipped (not in notify.imessage) pane_id=%2"),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// The app route for the chief's turn-end updates
// ---------------------------------------------------------------------------

/// What `FakeAwayText::summarise` returns for every turn.
pub(crate) const FAKE_SUMMARY: &str = "The pods are green.";
/// What `FakeAwayText::machine_name` returns.
pub(crate) const FAKE_MACHINE: &str = "mac";

/// An `AppRoute` that records each `(event_id, text)` it is handed and answers with the
/// delivery it is set to, or with an error, as a failed thread write does.
pub(crate) struct FakeApp {
    answer: Mutex<Result<AppDelivery, String>>,
    delivered: Mutex<Vec<(String, String)>>,
    reply_states: Mutex<Vec<(String, AppReplyState)>>,
    reply_state_fails: bool,
}

impl FakeApp {
    pub(crate) fn new(delivery: AppDelivery) -> Self {
        Self::answering(Ok(delivery))
    }

    /// Every `deliver` fails, as when the thread cannot be written.
    pub(crate) fn failing() -> Self {
        Self::answering(Err("app thread write failed: store is failing".into()))
    }

    fn answering(answer: Result<AppDelivery, String>) -> Self {
        Self {
            answer: Mutex::new(answer),
            delivered: Mutex::new(Vec::new()),
            reply_states: Mutex::new(Vec::new()),
            reply_state_fails: false,
        }
    }

    /// The app, with every `set_reply_state` failing, as when the thread cannot be written.
    pub(crate) fn failing_reply_states(self) -> Self {
        Self {
            reply_state_fails: true,
            ..self
        }
    }

    /// Each `(event_id, text)` delivered, oldest first.
    pub(crate) fn delivered(&self) -> Vec<(String, String)> {
        self.delivered.lock().unwrap().clone()
    }

    /// Each `(event_id, state)` that `set_reply_state` was asked to set, oldest first,
    /// including the calls that failed.
    pub(crate) fn reply_states(&self) -> Vec<(String, AppReplyState)> {
        self.reply_states.lock().unwrap().clone()
    }
}

impl AppRoute for FakeApp {
    fn deliver(&self, event_id: &str, text: &str) -> Result<AppDelivery, String> {
        let answer = self.answer.lock().unwrap().clone();
        if answer.is_ok() {
            self.delivered
                .lock()
                .unwrap()
                .push((event_id.to_string(), text.to_string()));
        }
        answer
    }

    fn set_reply_state(&self, event_id: &str, state: AppReplyState) -> Result<(), String> {
        self.reply_states
            .lock()
            .unwrap()
            .push((event_id.to_string(), state));
        if self.reply_state_fails {
            return Err("app reply state write failed: store is failing".into());
        }
        Ok(())
    }
}

/// An `AwayText` that records each text sent and never reaches a real channel.
#[derive(Default)]
pub(crate) struct FakeAwayText {
    sends: Mutex<Vec<String>>,
}

impl FakeAwayText {
    /// Each text sent, oldest first.
    pub(crate) fn sends(&self) -> Vec<String> {
        self.sends.lock().unwrap().clone()
    }
}

impl AwayText for FakeAwayText {
    fn machine_name(&self) -> String {
        FAKE_MACHINE.to_string()
    }

    fn summarise(&self, _turn: &TurnCompleted) -> String {
        FAKE_SUMMARY.to_string()
    }

    fn send(&self, text: &str) -> Result<(), String> {
        self.sends.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

fn notify_chief_turn(
    environment: &FakeEnvironment,
    app: &FakeApp,
    away: &FakeAwayText,
    turn: &TurnCompleted,
) -> Result<DeliveryOutcome, String> {
    notify_with_app(
        turn,
        "event-1",
        "trace",
        &NotifyState::default(),
        &TrackedAgents::default(),
        environment,
        Some(&ChiefApp {
            app,
            away,
            away_allowed: true,
        }),
    )
}

#[test]
fn locked_chief_turn_goes_to_the_app_and_not_the_away_channel() {
    let environment = FakeEnvironment::locked("");
    let app = FakeApp::new(AppDelivery::Pushed);
    let away = FakeAwayText::default();

    let outcome = notify_chief_turn(&environment, &app, &away, &turn("%1", "report"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert!(environment.delivered().is_empty());
    assert_eq!(
        app.delivered(),
        [("event-1".to_string(), FAKE_SUMMARY.to_string())]
    );
    assert!(away.sends().is_empty());
    assert!(environment.reply_targets().is_empty());
}

#[test]
fn chief_turn_not_pushed_goes_to_imessage_once_with_the_prefix() {
    let environment = FakeEnvironment::locked("");
    let app = FakeApp::new(AppDelivery::NotPushed);
    let away = FakeAwayText::default();

    let outcome = notify_chief_turn(&environment, &app, &away, &turn("%1", "report"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert!(environment.delivered().is_empty());
    assert_eq!(app.delivered().len(), 1);
    let sends = away.sends();
    assert_eq!(sends.len(), 1);
    assert!(sends[0].starts_with("(app unreachable) "), "{sends:?}");
    assert_eq!(
        sends[0],
        format!(
            "(app unreachable) {}",
            agent_message_text(FAKE_MACHINE, "label-%1", FAKE_SUMMARY)
        )
    );
    assert_eq!(environment.reply_targets(), ["%1"]);
}

#[test]
fn unlocked_chief_turn_is_unchanged_by_the_app() {
    let environment = FakeEnvironment::new("");
    let app = FakeApp::new(AppDelivery::Pushed);
    let away = FakeAwayText::default();

    let outcome = notify_chief_turn(&environment, &app, &away, &turn("%1", "report"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Tts, "%1".to_string())]
    );
    assert!(app.delivered().is_empty());
    assert!(away.sends().is_empty());
}

#[test]
fn chief_turn_whose_thread_write_fails_is_an_error_and_sends_nothing() {
    let environment = FakeEnvironment::locked("");
    let app = FakeApp::failing();
    let away = FakeAwayText::default();

    let outcome = notify_chief_turn(&environment, &app, &away, &turn("%1", "report"));

    assert!(outcome.is_err());
    assert!(away.sends().is_empty());
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());
}

/// The `harold.notifications` count for an agent, channel and outcome.
fn notified(agent: &str, channel: &str, outcome: &str) -> u64 {
    crate::telemetry::test_support::exact_counter_value(
        "harold.notifications",
        &[("agent", agent), ("channel", channel), ("outcome", outcome)],
    )
    .unwrap_or(0)
}

/// A folder tree no other test uses, so this test's counts are its own.
fn unique_tree() -> String {
    format!("/work/obs-{}", uuid::Uuid::new_v4().simple())
}

fn pods_in(tree: &str) -> TrackedAgents {
    TrackedAgents::in_folders(&[
        ("%1", &format!("{tree}/chief-of-pods")),
        ("%2", &format!("{tree}/harold")),
    ])
}

fn only_chief_listed(tree: &str) -> String {
    format!(
        "[notify]\nchief = \"{tree}/chief-of-pods\"\nimessage = [\"{tree}/chief-of-pods\"]\nmachine_audio_output = [\"{tree}/chief-of-pods\"]"
    )
}

#[test]
fn an_update_a_list_holds_back_counts_kept_and_logs_the_fields() {
    for (locked, channel, line) in [
        (
            true,
            "imessage",
            "notification skipped (not in notify.imessage) pane_id=%2",
        ),
        (
            false,
            "machine_audio_output",
            "spoken update left out (not in notify.machine_audio_output) pane_id=%2",
        ),
    ] {
        let tree = unique_tree();
        let environment = if locked {
            FakeEnvironment::locked(&only_chief_listed(&tree))
        } else {
            FakeEnvironment::new(&only_chief_listed(&tree))
        };
        let logs = CapturedLogs::start();

        let outcome = notify_turn_among(
            &environment,
            &NotifyState::default(),
            &pods_in(&tree),
            &turn("%2", "SECRET-PROMPT"),
        );

        assert_eq!(
            outcome,
            Ok(DeliveryOutcome::HeldBack {
                reason: "not_listed"
            })
        );
        let text = logs.text();
        assert!(text.contains(line), "{text}");
        for field in [
            format!("channel=\"{channel}\""),
            "kind=\"turn_end_update\"".to_string(),
            "reason=\"not_listed\"".to_string(),
            format!("agent=\"{tree}/harold\""),
        ] {
            assert!(text.contains(&field), "{field} in {text}");
        }
        assert!(!text.contains("SECRET-PROMPT"), "{text}");
        assert_eq!(notified(&format!("{tree}/harold"), channel, "kept"), 1);
        assert_eq!(notified(&format!("{tree}/harold"), channel, "sent"), 0);
    }
}

#[test]
fn a_delivered_update_counts_sent_on_the_channel_that_took_it() {
    let tree = unique_tree();
    let chief = format!("{tree}/chief-of-pods");

    let desk = FakeEnvironment::new(&format!("[notify]\nchief = \"{chief}\""));
    // A pane that is not the chief, so the desk speaks it.
    let others = TrackedAgents::in_folders(&[("%1", &chief), ("%2", &format!("{tree}/harold"))]);
    notify_turn_among(&desk, &NotifyState::default(), &others, &turn("%2", "a")).unwrap();
    assert_eq!(
        notified(&format!("{tree}/harold"), "machine_audio_output", "sent"),
        1
    );

    let phone = FakeEnvironment::locked(&format!("[notify]\nchief = \"{chief}\""));
    notify_turn_among(&phone, &NotifyState::default(), &others, &turn("%2", "b")).unwrap();
    assert_eq!(notified(&format!("{tree}/harold"), "imessage", "sent"), 1);

    let app = FakeApp::new(AppDelivery::Pushed);
    let away = FakeAwayText::default();
    let chief_environment = FakeEnvironment::locked(&format!("[notify]\nchief = \"{chief}\""));
    let chief_agents = TrackedAgents::in_folders(&[("%1", &chief)]);
    notify_with_app(
        &turn("%1", "c"),
        "event-1",
        "trace",
        &NotifyState::default(),
        &chief_agents,
        &chief_environment,
        Some(&ChiefApp {
            app: &app,
            away: &away,
            away_allowed: true,
        }),
    )
    .unwrap();
    assert_eq!(notified(&chief, "web_app", "sent"), 1);
}

#[test]
fn what_the_chief_rule_the_mute_the_pause_and_a_message_this_turn_hold_counts_neither() {
    let tree = unique_tree();
    let chief = format!("{tree}/chief-of-pods");
    let overrides = format!("[notify]\nchief = \"{chief}\"");
    let agents = TrackedAgents::in_folders(&[("%1", &chief)]);

    // The chief's own update is not spoken at the desk.
    let logs = CapturedLogs::start();
    let desk = FakeEnvironment::new(&overrides);
    let outcome = notify_turn_among(&desk, &NotifyState::default(), &agents, &turn("%1", "a"));
    assert_eq!(outcome, Ok(DeliveryOutcome::HeldBack { reason: "chief" }));
    let text = logs.text();
    assert!(
        text.contains("spoken update left out (chief) pane_id=%1"),
        "{text}"
    );
    assert!(text.contains("reason=\"chief\""), "{text}");

    // The voice mute and the pause.
    super::set_tts_muted(true);
    notify_turn_among(&desk, &NotifyState::default(), &agents, &turn("%1", "b")).unwrap();
    super::set_tts_muted(false);
    super::set_messaging_paused(true);
    let phone = FakeEnvironment::locked(&overrides);
    notify_turn_among(&phone, &NotifyState::default(), &agents, &turn("%1", "c")).unwrap();
    super::set_messaging_paused(false);

    // A message sent this turn.
    let state = NotifyState::default();
    state.mark_requested_message("%1", phone.now());
    let outcome = notify_turn_among(&phone, &state, &agents, &turn("%1", "d"));
    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));

    for channel in ["imessage", "machine_audio_output", "web_app"] {
        for outcome in ["sent", "kept"] {
            assert_eq!(notified(&chief, channel, outcome), 0, "{channel} {outcome}");
        }
    }
}

#[test]
fn an_update_held_back_on_a_channel_not_set_up_counts_nothing_but_is_still_logged() {
    let tree = unique_tree();
    let environment = FakeEnvironment::locked(&only_chief_listed(&tree));
    environment.set_not_set_up(crate::settings::Channel::Imessage);
    let logs = CapturedLogs::start();

    notify_turn_among(
        &environment,
        &NotifyState::default(),
        &pods_in(&tree),
        &turn("%2", "x"),
    )
    .unwrap();

    assert!(
        logs.text()
            .contains("notification skipped (not in notify.imessage)")
    );
    assert_eq!(notified(&format!("{tree}/harold"), "imessage", "kept"), 0);
}
