use std::time::Duration;

use super::fake::{CapturedLogs, FakeEnvironment, turn};
use super::left_out::TrackedAgents;
use super::{DeliveryOutcome, NotifyState, OutboundChannel, notify};
use crate::store::TurnCompleted;

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
// Rule B: updates from one folder only
// ---------------------------------------------------------------------------

const ONLY_FROM_CHIEF: &str = "[notify]\naway_messages_only_from = \"/work/chief-of-pods\"";

/// Pane %1 runs an agent in the update folder, pane %2 runs one in another folder.
fn chief_and_pod() -> TrackedAgents {
    TrackedAgents::in_folders(&[("%1", "/work/chief-of-pods"), ("%2", "/work/harold")])
}

#[test]
fn pane_outside_the_update_folder_sends_nothing_to_the_phone() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();
    let finished = turn("%2", "build it");

    let outcome = notify_turn_among(&environment, &state, &chief_and_pod(), &finished);

    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));
    assert!(environment.delivered().is_empty());
    assert!(environment.reply_targets().is_empty());

    // Recorded in the duplicate check: the repeat is left out even once the agent in the
    // update folder has gone.
    let pod_only = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
    assert_eq!(
        notify_turn_among(&environment, &state, &pod_only, &finished),
        Ok(DeliveryOutcome::Skipped)
    );
    assert!(environment.delivered().is_empty());
}

#[test]
fn pane_in_the_update_folder_sends_its_update() {
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
fn every_pane_sends_its_update_while_no_agent_is_in_the_update_folder() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();
    let pod_only = TrackedAgents::in_folders(&[("%2", "/work/harold")]);

    let outcome = notify_turn_among(&environment, &state, &pod_only, &turn("%2", "build it"));

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
}

#[test]
fn update_at_the_desk_is_spoken_for_a_pane_outside_the_update_folder() {
    let environment = FakeEnvironment::new(ONLY_FROM_CHIEF);
    let state = NotifyState::default();

    let outcome = notify_turn_among(
        &environment,
        &state,
        &chief_and_pod(),
        &turn("%2", "build it"),
    );

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Tts, "%2".to_string())]
    );
}

#[test]
fn without_the_setting_every_pane_sends_its_update() {
    let environment = FakeEnvironment::locked("");
    let state = NotifyState::default();

    let outcome = notify_turn_among(
        &environment,
        &state,
        &chief_and_pod(),
        &turn("%2", "build it"),
    );

    assert_eq!(outcome, Ok(DeliveryOutcome::Delivered));
}

#[test]
fn agent_in_the_update_folder_that_messaged_this_turn_gets_no_update() {
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

// ---------------------------------------------------------------------------
// The log line for a left-out update names the pane
// ---------------------------------------------------------------------------

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
fn update_left_out_by_the_folder_rule_is_logged_with_its_pane() {
    let environment = FakeEnvironment::locked(ONLY_FROM_CHIEF);
    let state = NotifyState::default();
    let logs = CapturedLogs::start();

    let outcome = notify_turn_among(
        &environment,
        &state,
        &chief_and_pod(),
        &turn("%2", "build it"),
    );

    assert_eq!(outcome, Ok(DeliveryOutcome::Skipped));
    let text = logs.text();
    assert!(
        text.contains(
            "notification skipped (updates come from /work/chief-of-pods only) pane_id=%2"
        ),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// The quiet rule: the chief's panes, build workers and pod sessions are not spoken
// ---------------------------------------------------------------------------

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-quiet-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// `<this>/<name>`, made, as a string.
    fn folder(&self, name: &str) -> String {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.to_string_lossy().into_owned()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(folder: &str, relative: &str, text: &str) {
    let path = std::path::Path::new(folder).join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A chief %1 in `chief`; a build worker %2 in a worktree whose baton file names it; a
/// pod session %3 in the product's base checkout, listed in its `agent-panes`; a session
/// %4 the user drives in the same base checkout; and %5, the user's own session in the
/// worktree, which the baton file does not name.
fn pod(directory: &TestDirectory) -> (String, TrackedAgents) {
    let chief = directory.folder("chief-of-pods");
    let product = directory.folder("harold/main");
    let worktree = directory.folder("harold/chief-voice");
    write(&worktree, ".superpowers/baton/agent-pane", "%2\n");
    write(&product, ".superpowers/pod/agent-panes", "%3\n%99\n");
    let agents = TrackedAgents::in_folders(&[
        ("%1", chief.as_str()),
        ("%2", worktree.as_str()),
        ("%3", product.as_str()),
        ("%4", product.as_str()),
        ("%5", worktree.as_str()),
    ]);
    (chief, agents)
}

#[test]
fn while_the_chief_runs_only_the_sessions_the_user_drives_are_spoken() {
    let directory = TestDirectory::new();
    let (chief, agents) = pod(&directory);
    let environment =
        FakeEnvironment::new(&format!("[notify]\naway_messages_only_from = \"{chief}\""));
    let logs = CapturedLogs::start();

    for pane in ["%1", "%2", "%3", "%4", "%5"] {
        let state = NotifyState::default();
        notify_turn_among(&environment, &state, &agents, &turn(pane, "build it")).unwrap();
    }

    assert_eq!(
        environment.delivered(),
        [
            (OutboundChannel::Tts, "%4".to_string()),
            (OutboundChannel::Tts, "%5".to_string()),
        ]
    );
    let text = logs.text();
    assert!(text.contains("spoken update left out (chief) pane_id=%1"));
    assert!(text.contains("spoken update left out (build worker) pane_id=%2"));
    assert!(text.contains("spoken update left out (pod session) pane_id=%3"));
}

#[test]
fn with_no_agent_in_the_chiefs_folder_every_pane_is_spoken() {
    let directory = TestDirectory::new();
    let (chief, agents) = pod(&directory);
    let environment = FakeEnvironment::new(&format!(
        "[notify]\naway_messages_only_from = \"{chief}/elsewhere\""
    ));

    for pane in ["%1", "%2", "%3", "%4", "%5"] {
        notify_turn_among(
            &environment,
            &NotifyState::default(),
            &agents,
            &turn(pane, "x"),
        )
        .unwrap();
    }

    assert_eq!(environment.delivered().len(), 5);
}

#[test]
fn quiet_rule_does_not_change_updates_to_the_phone() {
    let directory = TestDirectory::new();
    let (chief, agents) = pod(&directory);
    let environment =
        FakeEnvironment::locked(&format!("[notify]\naway_messages_only_from = \"{chief}\""));

    notify_turn_among(
        &environment,
        &NotifyState::default(),
        &agents,
        &turn("%1", "x"),
    )
    .unwrap();

    assert_eq!(
        environment.delivered(),
        [(OutboundChannel::Away, "%1".to_string())]
    );
}
