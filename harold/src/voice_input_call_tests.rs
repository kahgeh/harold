//! More of a call on the fakes of `voice_input_tests.rs`: the worker's health as it changes,
//! a sound the sink cannot play, a turn said before a bye, and a second START.

use super::*;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_fails_to_load_mid_turn_ends_the_call_at_once() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.start().await;
    h.feed(loud(1000));
    settle().await;

    h.set_health(Health::Gone(WorkerError::FailedToLoad));
    settle().await;

    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.tts(), [VOICE_FAILED]);
    h.feed(quiet(1200));
    settle().await;
    assert!(h.worker().transcribed.is_empty());
    assert!(h.keys().is_empty());
    assert!(logs.text().contains("call ended reason=\"voice_failed\""));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_in_hand_when_the_load_fails_is_dropped() {
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.script([Heard::TimeOut]);
    h.start().await;
    h.speak().await;
    assert_eq!(h.state(), CallState::Transcribing);

    h.set_health(Health::Gone(WorkerError::FailedToLoad));
    settle().await;

    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.status().last_result, Some(TurnResult::Failed));
    assert!(h.keys().is_empty());
    assert!(h.call_dirs().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_exits_while_idle_between_turns_is_restarted_at_once() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.answer("answer").await;

    h.set_health(Health::Gone(WorkerError::Exited));
    settle().await;

    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.worker().closes, 1);
    assert_eq!(h.tts(), [VOICE_STOPPED]);
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(
        logs.text()
            .matches("voice worker exited reason=\"exited\"")
            .count(),
        1
    );
    past_the_tail().await;
    h.speak().await;
    assert_eq!(h.keys().len(), 4);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn one_death_seen_twice_counts_once() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([Heard::ExitLater]);
    h.start().await;

    // The health shows the death first: the worker is started again at once.
    h.speak().await;
    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.tts(), [VOICE_STOPPED]);

    // Then the request in hand answers `Exited`: the turn is dropped, nothing more.
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.status().last_result, Some(TurnResult::Failed));
    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.tts(), [VOICE_STOPPED]);
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(logs.text().matches("voice worker exited").count(), 1);
    assert!(h.turn_dirs().is_empty());

    h.speak().await;
    assert_eq!(h.keys(), typed("%2", "hello there"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_never_says_ready_fails_its_load_after_120_s() {
    let folder = std::env::temp_dir().join(format!("harold-call-load-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let script = folder.join("silent-worker");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 600\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let command = WorkerCommand {
        python: script.display().to_string(),
        stt_model: "stt".into(),
        tts_model: "tts".into(),
        voice: "af_heart".into(),
        lang_code: "a".into(),
    };
    let worker = ProcessWorker::start(&command, &folder).unwrap();
    let started = Instant::now();
    let mut health = worker.health();

    let gone = *health
        .wait_for(|health| matches!(health, Health::Gone(_)))
        .await
        .unwrap();

    assert_eq!(gone, Health::Gone(WorkerError::FailedToLoad));
    assert!(started.elapsed() >= super::worker::LOAD_LIMIT);
    worker.close().await;
    std::fs::remove_dir_all(&folder).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_sound_the_sink_cannot_play_is_said_through_tts() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.sink().fails.store(true, Ordering::SeqCst);

    let said = h.say("answer", Speaker::Chief);
    settle().await;

    assert_eq!(said.await.unwrap(), Some(true));
    assert!(h.played().is_empty());
    assert_eq!(h.tts(), ["answer"]);
    assert_eq!(h.state(), CallState::Listening);
    // Muted while the `[tts]` command said it, and for 0.3 s after.
    h.speak().await;
    assert_eq!(h.worker().transcribed.len(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_before_bye_waits_for_a_busy_chief_after_the_call_ends() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("approve q19, bye")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);

    h.speak().await;

    assert_eq!(h.played(), [BYE]);
    assert_eq!(h.state(), CallState::Off);
    assert!(h.call_dirs().is_empty());
    sleep(Duration::from_secs(30)).await;
    assert!(h.keys().is_empty());
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.keys(), typed("%2", "approve q19"));
    let text = logs.text();
    assert!(
        text.contains("spoken prompt typed pane_id=%2 words=2"),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_before_bye_for_a_chief_busy_past_2_minutes_is_dropped() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("approve q19, bye")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    assert_eq!(h.state(), CallState::Off);

    sleep(Duration::from_secs(125)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(5)).await;

    assert!(h.keys().is_empty());
    let text = logs.text();
    assert!(
        text.contains("spoken prompt dropped reason=\"chief_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_second_start_while_the_first_opens_answers_the_call_it_opens() {
    let h = Harness::new();

    let (first, second) = tokio::join!(h.calls.start(), h.calls.start());

    assert_eq!(first.state, CallState::Listening);
    assert_eq!(second.state, CallState::Listening);
    assert_eq!(h.ports.state().sources, 1);
    assert_eq!(h.worker().starts, 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_while_transcribing_still_plays_the_answer_to_the_typed_turn() {
    let h = Harness::new();
    h.script([Heard::Slow("hello there".to_string())]);
    h.start().await;
    h.speak().await;
    assert_eq!(h.state(), CallState::Transcribing);

    h.calls.talk_now();
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.keys(), typed("%2", "hello there"));
    assert_eq!(h.state(), CallState::Waiting);

    let said = h.say("answer", Speaker::Chief);
    assert_eq!(said.await.unwrap(), Some(true));
    settle().await;
    assert_eq!(h.played(), ["answer"]);
    assert_eq!(h.state(), CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_while_a_turn_waits_for_a_busy_chief_still_plays_the_answer() {
    let h = Harness::new();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    assert_eq!(h.played(), [CHIEF_BUSY]);

    h.calls.talk_now();
    settle().await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.keys(), typed("%2", "hello there"));
    assert_eq!(h.state(), CallState::Waiting);

    let said = h.say("answer", Speaker::Chief);
    assert_eq!(said.await.unwrap(), Some(true));
    settle().await;
    assert_eq!(h.played(), [CHIEF_BUSY, "answer"]);
    assert_eq!(h.state(), CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_while_transcribing_nothing_heard_still_says_sorry_at_once() {
    let h = Harness::new();
    h.script([Heard::Slow(String::new())]);
    h.start().await;
    h.speak().await;

    h.calls.talk_now();
    sleep(Duration::from_millis(1100)).await;

    assert_eq!(h.played(), [NOTHING_HEARD]);
    assert_eq!(h.status().last_result, Some(TurnResult::NothingHeard));
    assert_eq!(h.state(), CallState::Listening);
}
