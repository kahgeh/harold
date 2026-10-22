//! Hand-overs on the fake chief pane: typed at once, also into a busy chief; held while the
//! input box holds text; given up after 30 minutes with a line in the thread.

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::outbound::fake::CapturedLogs;
use crate::voice_input::tests::{FakeAnswerer, FakePorts, FakeThread, settle};
use std::time::Duration;
use tokio::time::sleep;

const HOLDS_TEXT: &str = "────\n❯ half a sentence\n────\n  -- INSERT --\n";

fn ports_with_thread() -> (Arc<FakePorts>, FakeThread) {
    let ports = FakePorts::new();
    let thread = FakeThread::default();
    ports.with_answering(FakeAnswerer::default(), thread.clone());
    (Arc::new(ports), thread)
}

fn keys_of(ports: &FakePorts) -> Vec<String> {
    ports
        .fake_panes()
        .keys()
        .iter()
        .map(|key| format!("{key:?}"))
        .collect()
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_is_typed_at_once_into_a_busy_chief_and_numbered() {
    let logs = CapturedLogs::start();
    let (ports, _) = ports_with_thread();
    ports
        .fake_panes()
        .set(|s| s.view.panes[0].state = EffectiveAgentState::Busy);
    let mut hand_overs = HandOvers::default();

    let first = hand_overs.start(Arc::clone(&ports), "rerun the\nharold build");
    settle().await;
    let second = hand_overs.start(Arc::clone(&ports), "and tell me when");
    sleep(Duration::from_secs(1)).await;
    settle().await;

    assert_eq!((first.as_str(), second.as_str()), ("H1", "H2"));
    let keys = keys_of(&ports);
    assert_eq!(keys.len(), 4, "{keys:?}");
    assert!(
        keys[0].contains("🎙 hand-over H1: rerun the harold build"),
        "{keys:?}"
    );
    assert!(
        keys[2].contains("🎙 hand-over H2: and tell me when"),
        "{keys:?}"
    );
    let facts = hand_overs.facts();
    assert!(facts.iter().all(|fact| fact.state == HandOverState::Typed));
    assert_eq!(facts[0].request, "rerun the harold build");
    let text = logs.text();
    assert!(
        text.contains("hand-over typed hand_over_id=H1 pane_id=%2 typed_while_busy=true wait_ms=0"),
        "{text}"
    );
    assert!(!text.contains("harold build"), "{text}");
    assert!(!text.contains("tell me when"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_waits_while_the_box_holds_text_and_is_typed_once_it_is_empty() {
    let logs = CapturedLogs::start();
    let (ports, thread) = ports_with_thread();
    ports
        .fake_panes()
        .set(|s| s.screen = Some(HOLDS_TEXT.to_string()));
    let mut hand_overs = HandOvers::default();

    hand_overs.start(Arc::clone(&ports), "rerun the build");
    sleep(Duration::from_secs(12)).await;

    assert!(keys_of(&ports).is_empty());
    assert_eq!(hand_overs.facts()[0].state, HandOverState::Waiting);
    ports.fake_panes().set(|s| {
        s.screen = Some("────\n❯ \n────\n  -- INSERT --\n".to_string());
    });
    sleep(Duration::from_secs(5)).await;
    settle().await;

    assert_eq!(keys_of(&ports).len(), 2);
    assert_eq!(hand_overs.facts()[0].state, HandOverState::Typed);
    let text = logs.text();
    assert_eq!(
        text.matches("hand-over waiting hand_over_id=H1 reason=\"input_box_holds_text\"")
            .count(),
        1,
        "{text}"
    );
    assert!(text.contains("hand-over typed hand_over_id=H1"), "{text}");
    assert!(
        text.contains("typed_while_busy=false wait_ms=15000"),
        "{text}"
    );
    assert!(thread.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_not_typed_in_30_minutes_is_given_up_and_the_thread_is_told() {
    let logs = CapturedLogs::start();
    let (ports, thread) = ports_with_thread();
    ports
        .fake_panes()
        .set(|s| s.screen = Some(HOLDS_TEXT.to_string()));
    let mut hand_overs = HandOvers::default();

    hand_overs.start(Arc::clone(&ports), "rerun the build");
    sleep(GIVE_UP - Duration::from_secs(10)).await;
    settle().await;
    assert!(thread.0.lock().unwrap().is_empty());
    sleep(Duration::from_secs(20)).await;
    settle().await;

    assert!(keys_of(&ports).is_empty());
    let told = thread.0.lock().unwrap().clone();
    assert_eq!(
        told,
        [(
            Some("%2".to_string()),
            "Harold couldn't pass hand-over H1 to the chief.".to_string()
        )]
    );
    let text = logs.text();
    assert!(
        text.contains("WARN")
            && text.contains("hand-over not typed hand_over_id=H1 reason=\"input_box_holds_text\""),
        "{text}"
    );
    assert!(!text.contains("rerun the build"), "{text}");
    // It stopped trying.
    ports.fake_panes().set(|s| {
        s.screen = Some("────\n❯ \n────\n  -- INSERT --\n".to_string());
    });
    sleep(Duration::from_secs(60)).await;
    assert!(keys_of(&ports).is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn with_no_chief_running_a_hand_over_waits_and_the_give_up_has_no_pane_to_say_it_from() {
    let (ports, thread) = ports_with_thread();
    ports.fake_panes().set(|s| s.view.panes.clear());
    let mut hand_overs = HandOvers::default();

    hand_overs.start(Arc::clone(&ports), "rerun the build");
    sleep(GIVE_UP + Duration::from_secs(10)).await;
    settle().await;

    assert_eq!(
        thread.0.lock().unwrap().clone(),
        [(
            None,
            "Harold couldn't pass hand-over H1 to the chief.".to_string()
        )]
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_waits_for_a_chief_whose_state_is_unknown_and_for_a_pane_list_that_fails() {
    let (ports, _) = ports_with_thread();
    ports
        .fake_panes()
        .set(|s| s.view.panes[0].state = EffectiveAgentState::Unknown);
    let mut hand_overs = HandOvers::default();
    hand_overs.start(Arc::clone(&ports), "one");
    sleep(Duration::from_secs(11)).await;
    assert!(keys_of(&ports).is_empty());

    ports.fake_panes().set(|s| {
        s.view.panes[0].state = EffectiveAgentState::Busy;
        s.view.listing_failed = true;
    });
    sleep(Duration::from_secs(11)).await;
    assert!(keys_of(&ports).is_empty());

    ports.fake_panes().set(|s| s.view.listing_failed = false);
    sleep(Duration::from_secs(6)).await;
    settle().await;
    assert_eq!(keys_of(&ports).len(), 2);
}
