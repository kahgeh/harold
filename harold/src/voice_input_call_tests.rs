//! More of a call on the fakes of `voice_input_tests.rs`: the worker's health as it changes,
//! a sound the sink cannot play, a turn said before a bye, a second START, and the fixes of
//! the final review: the idle end, the mid-call mute end and the talk-now hold's limit.

use super::super::hand_over::{HandOverFact, HandOverState};
use super::super::phrases::{FROM_THE_CHIEF, PASSED_ON, phrase_file};
use super::super::wav::wav_bytes;
use super::answerer::{Failure, Remark, Who};
use super::*;
use tokio::time::sleep_until;

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
    h.script([Heard::TimeOut]);
    h.start().await;
    // The worker is started again and is loading when the user speaks.
    h.worker().starts_loading = true;
    h.set_health(Health::Gone(WorkerError::Exited));
    settle().await;
    past_the_tail().await;
    h.feed(quiet(500));
    settle().await;
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
        language: "en".into(),
        stt_pin: None,
        tts_pin: None,
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
async fn a_held_turn_joined_to_a_turn_before_bye_keeps_its_first_2_minutes() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("approve q19"), text("and q20, bye")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    sleep(Duration::from_secs(60)).await;
    h.speak().await;
    assert_eq!(h.played(), [CHIEF_BUSY, BYE]);
    assert_eq!(h.state(), CallState::Off);

    sleep(Duration::from_secs(65)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(10)).await;

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

    assert_eq!(first.state, CallState::Connecting);
    assert_eq!(second.state, CallState::Connecting);
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

// ---------------------------------------------------------------------------
// The final review's fixes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_exits_while_making_the_answer_still_plays_the_answer() {
    let h = Harness::new();
    h.worker().say_script.push_back(WorkerError::Exited);
    h.start().await;
    h.speak().await;

    let said = h.say("the answer", Speaker::Chief);
    assert_eq!(said.await.unwrap(), Some(true));
    settle().await;

    assert_eq!(h.worker().starts, 2);
    assert_eq!(h.worker().said, ["the answer", "the answer"]);
    assert_eq!(h.played(), ["the answer"]);
    assert_eq!(h.tts(), [VOICE_STOPPED]);
    assert_eq!(h.state(), CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn muting_the_voice_mid_call_ends_the_call_at_once() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    assert_eq!(h.state(), CallState::Waiting);

    h.ports.state().muted = true;
    h.feed(quiet(100));
    settle().await;

    let status = h.status();
    assert_eq!(status.state, CallState::Off);
    assert_eq!(status.last_result, Some(TurnResult::VoiceMuted));
    // No tick at 3 s, and no last words.
    sleep(Duration::from_secs(5)).await;
    assert!(h.played().is_empty());
    assert!(h.tts().is_empty());
    assert_eq!(h.awake_held(), 0);
    assert!(h.call_dirs().is_empty());
    let text = logs.text();
    assert!(text.contains("call ended reason=\"voice_muted\""), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_with_nothing_said_for_10_minutes_ends_with_bye() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;

    sleep(Duration::from_secs(10 * 60 - 1)).await;
    assert_eq!(h.state(), CallState::Listening);
    sleep(Duration::from_secs(2)).await;

    assert_eq!(h.played(), [BYE]);
    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.awake_held(), 0);
    assert!(h.call_dirs().is_empty());
    let text = logs.text();
    assert!(text.contains("call ended reason=\"idle\""), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_at_9_minutes_pushes_the_idle_end_to_19_minutes() {
    let h = Harness::new();
    h.start().await;
    let started = Instant::now();

    sleep(Duration::from_secs(9 * 60)).await;
    h.speak().await;
    h.answer("answer").await;

    sleep_until(started + Duration::from_secs(19 * 60 - 1)).await;
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(h.played(), ["answer"]);
    sleep_until(started + Duration::from_secs(19 * 60 + 1)).await;
    assert_eq!(h.played(), ["answer", BYE]);
    assert_eq!(h.state(), CallState::Off);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_transcript_is_not_kept_after_the_call_ends() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    assert_eq!(h.status().last_transcript.as_deref(), Some("hello there"));

    h.calls.stop().await;

    let status = h.status();
    assert_eq!(status.last_transcript, None);
    assert_eq!(status.last_result, Some(TurnResult::Typed));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_line_held_after_talk_now_plays_10_s_later_with_no_turn() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.sink().hold.store(true, Ordering::SeqCst);
    let said = h.say("a long answer", Speaker::Chief);
    settle().await;
    let update = h.say("update", Speaker::OtherAgent);
    settle().await;

    h.calls.talk_now();
    let held_at = Instant::now();
    assert_eq!(said.await.unwrap(), Some(true));
    h.sink().hold.store(false, Ordering::SeqCst);

    sleep_until(held_at + Duration::from_millis(9_900)).await;
    assert!(!update.is_finished());
    assert_eq!(h.played(), ["a long answer"]);
    sleep_until(held_at + Duration::from_millis(10_100)).await;
    assert!(update.is_finished(), "the held line still waits");
    assert_eq!(update.await.unwrap(), Some(true));
    let played = h.played_at();
    assert_eq!(played.len(), 2);
    assert_eq!(played[1].1, "update");
    assert_eq!(played[1].0 - held_at, Duration::from_secs(10));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_logs_its_start_and_its_end() {
    let logs = CapturedLogs::start();
    let h = Harness::new();

    h.start().await;
    assert!(logs.text().contains("call started"), "{}", logs.text());
    assert!(!logs.text().contains("call ended"), "{}", logs.text());

    h.calls.stop().await;
    let text = logs.text();
    assert!(text.contains("call ended reason=\"stopped\""), "{text}");
}

#[test]
fn the_sweep_removes_only_call_folders() {
    let calls = std::env::temp_dir().join(format!("harold-sweep-{}", uuid::Uuid::new_v4()));
    for dir in ["call-1", "call-2/turn-1", "keep"] {
        std::fs::create_dir_all(calls.join(dir)).unwrap();
    }
    std::fs::write(calls.join("call-2/turn-1/turn.wav"), "x").unwrap();
    std::fs::write(calls.join("call-file"), "x").unwrap();
    std::fs::write(calls.join("notes.txt"), "x").unwrap();

    crate::voice_input::call::sweep_call_folders(&calls);

    let mut left: Vec<String> = std::fs::read_dir(&calls)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["call-file", "keep", "notes.txt"]);
    std::fs::remove_dir_all(&calls).unwrap();

    // A base folder that is not there is left alone.
    crate::voice_input::call::sweep_call_folders(&calls);
    assert!(!calls.exists());
}

#[test]
fn a_call_folder_already_gone_is_removed_quietly_and_a_failed_removal_warns() {
    use std::os::unix::fs::PermissionsExt;

    let logs = CapturedLogs::start();
    let base = std::env::temp_dir().join(format!("harold-remove-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();

    crate::voice_input::call::remove_call_folder(&base.join("call-gone"));
    assert!(
        !logs.text().contains("call folder not removed"),
        "{}",
        logs.text()
    );

    std::fs::create_dir_all(base.join("call-stuck")).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o500)).unwrap();
    crate::voice_input::call::remove_call_folder(&base.join("call-stuck"));
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    let text = logs.text();
    assert!(text.contains("call folder not removed error="), "{text}");
    std::fs::remove_dir_all(&base).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_whose_folder_went_before_it_ended_logs_no_warning() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    let dirs = h.call_dirs();
    assert_eq!(dirs.len(), 1);
    for dir in dirs {
        std::fs::remove_dir_all(dir).unwrap();
    }

    h.calls.stop().await;

    let text = logs.text();
    assert!(text.contains("call ended reason=\"stopped\""), "{text}");
    assert!(!text.contains("call folder not removed"), "{text}");
}

// ---------------------------------------------------------------------------
// The call turn's spans
// ---------------------------------------------------------------------------

type Span = opentelemetry_sdk::trace::SpanData;

/// Every exported span of the trace that holds the `harold.call_transcribe` span whose clip
/// lasts `audio_ms`: a test gives its speech a length no other test uses.
fn trace_of_clip(audio_ms: u64) -> Vec<Span> {
    let spans = test_support::spans();
    let own = spans
        .iter()
        .find(|span| {
            span.name == "harold.call_transcribe"
                && test_support::span_attr(span, "audio_ms").as_deref()
                    == Some(audio_ms.to_string().as_str())
        })
        .expect("the clip was transcribed");
    let trace = own.span_context.trace_id();
    spans
        .into_iter()
        .filter(|span| span.span_context.trace_id() == trace)
        .collect()
}

fn named<'a>(spans: &'a [Span], name: &str) -> Vec<&'a Span> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn root(spans: &[Span]) -> &Span {
    let roots: Vec<_> = spans
        .iter()
        .filter(|span| span.parent_span_id == opentelemetry::trace::SpanId::INVALID)
        .collect();
    assert_eq!(roots.len(), 1, "one root: {roots:?}");
    assert_eq!(roots[0].name, "harold.call_turn");
    roots[0]
}

fn is_child_of(span: &Span, parent: &Span) -> bool {
    span.parent_span_id == parent.span_context.span_id()
}

fn attr(span: &Span, key: &str) -> Option<String> {
    test_support::span_attr(span, key)
}

/// No attribute of `spans` holds `secrets`, a folder of the call or a file name.
fn holds_no_words_or_paths(spans: &[Span], secrets: &[&str]) {
    for span in spans {
        for kv in &span.attributes {
            let value = kv.value.as_str().into_owned();
            for secret in secrets {
                assert!(
                    !value.contains(secret),
                    "{}: {} = {value}",
                    span.name,
                    kv.key
                );
            }
            assert!(!value.contains('/'), "{}: {} = {value}", span.name, kv.key);
            assert!(
                !value.contains(".wav"),
                "{}: {} = {value}",
                span.name,
                kv.key
            );
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_from_speech_to_the_answer_played_is_one_trace_under_the_call_turn() {
    test_support::install();
    let h = Harness::new();
    let pane = own_chief_pane(&h);
    h.script([text("spoken words zebra")]);
    h.start().await;

    h.feed(loud(1340));
    h.feed(quiet(1200));
    settle().await;
    let (_, _, cx) = telemetry::take_prompt(&pane, time::OffsetDateTime::now_utc())
        .expect("the typed prompt was kept");
    h.answer("answer words zebra").await;
    h.calls.stop().await;
    settle().await;

    let spans = trace_of_clip(1340);
    let turn = root(&spans);
    assert_eq!(attr(turn, "pane_id").as_deref(), Some(pane.as_str()));
    assert_eq!(attr(turn, "call_from").as_deref(), Some("desk"));
    assert_eq!(attr(turn, "result").as_deref(), Some("typed"));
    let first_sound: u64 = attr(turn, "first_sound_ms")
        .expect("a first sound")
        .parse()
        .unwrap();
    let answer: u64 = attr(turn, "answer_ms").expect("an answer").parse().unwrap();
    assert!(first_sound <= answer);
    // The stop hook's span joins the trace through the context kept with the prompt.
    use opentelemetry::trace::TraceContextExt as _;
    assert_eq!(
        cx.expect("a context").span().span_context().trace_id(),
        turn.span_context.trace_id()
    );

    let speech = named(&spans, "harold.call_speech");
    assert_eq!(speech.len(), 1);
    assert!(is_child_of(speech[0], turn));
    assert_eq!(attr(speech[0], "cut").as_deref(), Some("quiet"));
    let transcribe = named(&spans, "harold.call_transcribe");
    assert_eq!(transcribe.len(), 1);
    assert!(is_child_of(transcribe[0], turn));
    assert_eq!(attr(transcribe[0], "result").as_deref(), Some("text"));
    assert!(attr(transcribe[0], "waited_ms").is_some());
    let voice = named(&spans, "harold.call_voice");
    assert_eq!(voice.len(), 1);
    assert!(is_child_of(voice[0], turn));
    assert_eq!(attr(voice[0], "source").as_deref(), Some("chief_line"));
    assert_eq!(attr(voice[0], "result").as_deref(), Some("made"));
    assert!(attr(voice[0], "waited_ms").is_some());
    assert!(attr(voice[0], "audio_ms").is_some());
    let plays = named(&spans, "harold.call_play");
    assert_eq!(plays.len(), 1);
    assert!(is_child_of(plays[0], turn));
    assert_eq!(attr(plays[0], "source").as_deref(), Some("chief_line"));
    assert_eq!(attr(plays[0], "call_from").as_deref(), Some("desk"));
    assert_eq!(attr(plays[0], "start_ms"), None);
    holds_no_words_or_paths(&spans, &["zebra", "spoken words", "answer words"]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_turn_end_update_is_the_answer_and_a_tick_is_no_first_sound() {
    test_support::install();
    let h = Harness::new();
    let pane = own_chief_pane(&h);
    h.start().await;
    h.feed(loud(1360));
    h.feed(quiet(1200));
    settle().await;

    // A busy chief: the tick plays at 3 s, then the update at the end of the turn.
    h.set_chief(EffectiveAgentState::Busy);
    sleep(Duration::from_millis(3500)).await;
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_millis(2700)).await;
    settle().await;
    past_the_tail().await;
    h.calls.stop().await;
    settle().await;

    let spans = trace_of_clip(1360);
    let turn = root(&spans);
    assert_eq!(attr(turn, "pane_id").as_deref(), Some(pane.as_str()));
    let plays: Vec<_> = named(&spans, "harold.call_play")
        .into_iter()
        .map(|play| attr(play, "source").unwrap())
        .collect();
    assert!(plays.contains(&"tick".to_string()), "{plays:?}");
    assert!(plays.contains(&"update".to_string()), "{plays:?}");
    let voices = named(&spans, "harold.call_voice");
    assert_eq!(voices.len(), 1);
    assert_eq!(attr(voices[0], "source").as_deref(), Some("update"));
    // The tick played first, and counts for neither number: both are the update's.
    assert_eq!(attr(turn, "first_sound_ms"), attr(turn, "answer_ms"));
    assert!(attr(turn, "answer_ms").is_some());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_with_no_speech_in_it_has_a_result_and_no_sound_numbers() {
    test_support::install();
    let h = Harness::new();
    h.script([Heard::Unlikely("mumble".to_string())]);
    h.start().await;

    h.feed(loud(1380));
    h.feed(quiet(1200));
    settle().await;
    past_the_tail().await;
    h.calls.stop().await;
    settle().await;

    let spans = trace_of_clip(1380);
    let turn = root(&spans);
    assert_eq!(attr(turn, "result").as_deref(), Some("nothing_heard"));
    assert_eq!(attr(turn, "first_sound_ms"), None);
    assert_eq!(attr(turn, "answer_ms"), None);
    let transcribe = named(&spans, "harold.call_transcribe");
    assert_eq!(transcribe.len(), 1);
    assert_eq!(attr(transcribe[0], "result").as_deref(), Some("no_speech"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_transcribe_the_worker_fails_ends_failed() {
    test_support::install();
    let h = Harness::new();
    h.script([Heard::Fail]);
    h.start().await;

    h.feed(loud(1400));
    h.feed(quiet(1200));
    settle().await;
    h.calls.stop().await;
    settle().await;

    let spans = trace_of_clip(1400);
    let transcribe = named(&spans, "harold.call_transcribe");
    assert_eq!(attr(transcribe[0], "result").as_deref(), Some("failed"));
    assert_eq!(attr(root(&spans), "result").as_deref(), Some("failed"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_forgotten_turn_ends_the_call_turn_as_forgotten() {
    test_support::install();
    let h = Harness::new();
    h.start().await;
    h.feed(loud(1420));
    settle().await;

    h.calls.forget_turn();
    settle().await;
    h.calls.stop().await;
    settle().await;

    let spans = test_support::spans();
    let forgotten: Vec<_> = spans
        .iter()
        .filter(|span| {
            span.name == "harold.call_turn" && attr(span, "result").as_deref() == Some("forgotten")
        })
        .collect();
    // Other tests forget turns too: each forgotten turn is its speech, cut by nothing.
    assert!(!forgotten.is_empty());
    for turn in forgotten {
        let speech: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "harold.call_speech" && is_child_of(span, turn))
            .collect();
        assert_eq!(speech.len(), 1);
        assert_eq!(attr(speech[0], "cut"), None);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn speech_cut_at_60_s_says_max_length() {
    test_support::install();
    let h = Harness::new();
    h.start().await;

    h.feed(loud(61_000));
    settle().await;
    h.calls.stop().await;
    settle().await;

    let cut: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.call_speech" && attr(span, "cut").as_deref() == Some("max_length")
        })
        .collect();
    assert_eq!(cut.len(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_kept_phrase_on_disk_has_a_play_span_and_no_voice_span() {
    test_support::install();
    let h = Harness::new();
    let kept = phrase_file(&h.ports.phrases_dir(), &h.ports.voice_name(), CHIEF_BUSY);
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, CHIEF_BUSY).unwrap();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);

    h.feed(loud(1440));
    h.feed(quiet(1200));
    settle().await;
    past_the_tail().await;
    h.calls.stop().await;
    settle().await;

    assert_eq!(h.played(), [CHIEF_BUSY]);
    assert!(h.worker().said.is_empty());
    let spans = trace_of_clip(1440);
    let turn = root(&spans);
    assert!(named(&spans, "harold.call_voice").is_empty());
    let plays = named(&spans, "harold.call_play");
    assert_eq!(plays.len(), 1);
    assert!(is_child_of(plays[0], turn));
    assert_eq!(attr(plays[0], "source").as_deref(), Some("phrase"));
    // It is the first sound of the turn.
    assert!(attr(turn, "first_sound_ms").is_some());
    assert_eq!(attr(turn, "answer_ms"), None);
    std::fs::remove_dir_all(kept.parent().unwrap()).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn another_agents_line_outside_a_turn_gets_root_spans() {
    test_support::install();
    let h = Harness::new();
    h.start().await;

    let said = h.say("from another agent", Speaker::OtherAgent);
    settle().await;
    assert_eq!(said.await.unwrap(), Some(true));
    h.calls.stop().await;
    settle().await;

    let spans = test_support::spans();
    let others: Vec<_> = spans
        .iter()
        .filter(|span| {
            matches!(span.name.as_ref(), "harold.call_voice" | "harold.call_play")
                && attr(span, "source").as_deref() == Some("other_agent")
        })
        .collect();
    assert!(others.len() >= 2, "{others:?}");
    for span in others {
        assert_eq!(span.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_phone_sound_offered_in_a_play_span_learns_when_it_started() {
    use super::super::phone::PhoneSink;
    use tracing::Instrument as _;

    test_support::install();
    let dir = std::env::temp_dir().join(format!("harold-play-span-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("sound.wav");
    std::fs::write(&wav, wav_bytes(&[0; 16_000], 16_000)).unwrap();
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let span = tracing::info_span!(
        target: telemetry::TRACE_TARGET,
        parent: None,
        "harold.call_play",
        source = "phone-start-test",
        start_ms = tracing::field::Empty
    );
    let playing = tokio::spawn({
        let sink = Arc::clone(&sink);
        let wav = wav.clone();
        async move { sink.play_said(&wav, &Said::Phrase(String::new())).await }.instrument(span)
    });
    settle().await;
    let offered = line
        .view()
        .borrow()
        .sound
        .clone()
        .expect("a sound is offered");

    line.played_at(&offered.id, Some(350));

    assert_eq!(playing.await.unwrap(), Ok(()));
    settle().await;
    let plays: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|span| attr(span, "source").as_deref() == Some("phone-start-test"))
        .collect();
    assert_eq!(plays.len(), 1);
    assert_eq!(attr(&plays[0], "start_ms").as_deref(), Some("350"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_chief_line_after_its_turn_ended_is_no_answer_of_the_next_turn() {
    test_support::install();
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    h.answer("first line").await;

    // The next turn is spoken, and the chief's late line arrives while it is.
    h.feed(loud(1500));
    settle().await;
    let late = h.say("late line", Speaker::Chief);
    settle().await;
    h.feed(quiet(1200));
    settle().await;
    past_the_tail().await;
    assert_eq!(late.await.unwrap(), Some(true));
    h.calls.stop().await;
    settle().await;

    let spans = trace_of_clip(1500);
    let turn = root(&spans);
    assert!(named(&spans, "harold.call_voice").is_empty());
    assert_eq!(attr(turn, "answer_ms"), None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_earlier_turns_queued_line_playing_does_not_end_the_next_turn() {
    test_support::install();
    let h = Harness::new();
    let pane = own_chief_pane(&h);
    h.script([text("turn a"), text("turn b")]);
    h.worker()
        .say_delays
        .extend([Duration::ZERO, Duration::from_secs(10)]);
    h.start().await;
    h.speak().await;

    // Both lines answer turn A. The second is slow to make; turn B is spoken meanwhile.
    let first = h.say("line one", Speaker::Chief);
    let second = h.say("line two", Speaker::Chief);
    settle().await;
    past_the_tail().await;
    h.feed(loud(1520));
    h.feed(quiet(1200));
    settle().await;
    sleep(Duration::from_secs(11)).await;
    settle().await;
    assert_eq!(first.await.unwrap(), Some(true));
    assert_eq!(second.await.unwrap(), Some(true));
    past_the_tail().await;
    h.answer("answer b").await;
    h.calls.stop().await;
    settle().await;

    let spans = trace_of_clip(1520);
    let turn = root(&spans);
    assert_eq!(attr(turn, "pane_id").as_deref(), Some(pane.as_str()));
    assert!(attr(turn, "answer_ms").is_some());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_busy_chief_logs_why_the_turn_is_held() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);

    h.speak().await;

    let text = logs.text();
    assert!(
        text.contains("spoken prompt held reason=\"agent_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_input_box_holding_text_logs_why_the_turn_is_held() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    let rule = "─".repeat(56);
    let screen = format!("⏺ Done.\n\n{rule}\n❯ half a sentence\n{rule}\n  -- INSERT --\n");
    h.ports.0.panes.set(|s| s.screen = Some(screen));

    h.speak().await;

    let text = logs.text();
    assert!(
        text.contains("spoken prompt held reason=\"input_box_holds_text\""),
        "{text}"
    );
    assert!(h.keys().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_before_bye_held_for_a_busy_chief_logs_why() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("approve q19, bye")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);

    h.speak().await;

    let text = logs.text();
    assert!(
        text.contains("spoken prompt held reason=\"agent_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_dropped_after_2_minutes_logs_the_last_busy_reason() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;
    // The reason changes while the turn waits: the newest one is the one logged.
    sleep(Duration::from_secs(30)).await;
    h.set_chief(EffectiveAgentState::Unknown);

    sleep(Duration::from_secs(100)).await;

    let text = logs.text();
    assert!(
        text.contains(
            "spoken prompt dropped reason=\"chief_busy\" busy_reason=\"agent_state_unknown\""
        ),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_passed_on_after_the_call_and_dropped_logs_the_busy_reason() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.script([text("approve q19, bye")]);
    h.start().await;
    h.set_chief(EffectiveAgentState::Busy);
    h.speak().await;

    sleep(Duration::from_secs(125)).await;

    let text = logs.text();
    assert!(
        text.contains("spoken prompt dropped reason=\"chief_busy\" busy_reason=\"agent_busy\""),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_with_only_quiet_blocks_ends_with_no_turn_cut_and_the_sound_it_read() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.feed(quiet(1500));
    settle().await;

    h.calls.stop().await;

    let text = logs.text();
    assert!(text.contains("call ended reason=\"stopped\""), "{text}");
    assert!(text.contains("heard_ms=2000"), "{text}");
    // The test feeds the microphone after the ringing.
    assert!(text.contains("first_heard_ms=3500"), "{text}");
    assert!(text.contains("background=30"), "{text}");
    assert!(text.contains("loudest=0"), "{text}");
    assert!(text.contains("loud_frames=0"), "{text}");
    assert!(text.contains("speech_cut=0"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_with_speech_ends_with_a_turn_cut_and_loud_frames() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.start().await;
    h.speak().await;

    h.calls.stop().await;

    let text = logs.text();
    assert!(text.contains("speech_cut=1"), "{text}");
    assert!(text.contains("heard_ms=2700"), "{text}");
    let loud_frames: usize = text
        .split("loud_frames=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(loud_frames >= 10, "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_first_block_after_a_slow_start_is_timed_from_the_call_start() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.calls.start().await;
    sleep(Duration::from_millis(3000)).await;
    h.feed(quiet(500));
    settle().await;

    h.calls.stop().await;

    let text = logs.text();
    assert!(text.contains("first_heard_ms=3000"), "{text}");
}

// ---------------------------------------------------------------------------
// The call answerer
// ---------------------------------------------------------------------------

fn answering_harness() -> (Harness, FakeAnswerer, FakeThread) {
    let h = Harness::new();
    let (answerer, thread) = (FakeAnswerer::default(), FakeThread::default());
    h.ports.with_answering(answerer.clone(), thread.clone());
    (h, answerer, thread)
}

fn script_runs(answerer: &FakeAnswerer, runs: impl IntoIterator<Item = Run>) {
    answerer.log().script.extend(runs);
}

fn run_now(answer: Answer) -> Run {
    Run {
        answer,
        after: Duration::ZERO,
    }
}

fn run_after(seconds: u64, answer: Answer) -> Run {
    Run {
        answer,
        after: Duration::from_secs(seconds),
    }
}

fn turn_spans(trace: opentelemetry::trace::TraceId) -> Vec<opentelemetry_sdk::trace::SpanData> {
    test_support::spans()
        .into_iter()
        .filter(|span| span.name == "harold.call_turn" && span.span_context.trace_id() == trace)
        .collect()
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_answerer_answers_a_turn_and_nothing_is_typed_into_the_chief() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(said("Nothing is building."))]);
    h.script([text("is anything building")]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.played(), ["Nothing is building."]);
    assert!(h.keys().is_empty());
    assert_eq!(answerer.log().asked.len(), 1);
    assert_eq!(answerer.log().asked[0].words, "is anything building");
    assert!(answerer.log().asked[0].call.remarks.is_empty());
    let status = h.status();
    assert_eq!(status.last_result, Some(TurnResult::Answered));
    assert_eq!(
        status.last_transcript.as_deref(),
        Some("is anything building")
    );
    assert_eq!(status.state, CallState::Listening);
    // No chief turn to wait for: no update line and no local model.
    assert!(h.ports.state().update_prompts.is_empty());
    sleep(Duration::from_secs(120)).await;
    assert_eq!(h.played(), ["Nothing is building."]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_answerer_turn_is_recorded_with_who_answered_how_and_how_fast() {
    test_support::install();
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [
            run_after(4, said("Nothing is building.")),
            run_now(handed_over("Passed to the chief.", "rerun the build")),
        ],
    );
    h.script([text("is anything building"), text("rerun the build")]);
    h.start().await;

    h.speak().await;
    sleep(Duration::from_secs(5)).await;
    settle().await;
    past_the_tail().await;
    h.speak().await;

    let asked = answerer.log().asked.clone();
    assert_eq!(asked.len(), 2);
    let first = turn_spans(asked[0].trace);
    assert_eq!(first.len(), 1);
    let attr = |span, key: &str| test_support::span_attr(span, key);
    assert_eq!(attr(&first[0], "answered_by").as_deref(), Some("answerer"));
    assert_eq!(attr(&first[0], "result").as_deref(), Some("answered"));
    assert_eq!(attr(&first[0], "call_from").as_deref(), Some("desk"));
    let answer_ms: u64 = attr(&first[0], "answer_ms").unwrap().parse().unwrap();
    assert!((4000..4200).contains(&answer_ms), "{answer_ms}");
    // No phrase comes before the answer: it is the turn's first sound.
    let first_sound_ms: u64 = attr(&first[0], "first_sound_ms").unwrap().parse().unwrap();
    assert_eq!(first_sound_ms, answer_ms);
    let second = turn_spans(asked[1].trace);
    assert_eq!(attr(&second[0], "result").as_deref(), Some("handed_over"));
    assert_eq!(attr(&second[0], "answered_by").as_deref(), Some("answerer"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_request_for_work_is_passed_on_typed_into_a_busy_chief_and_the_next_turn_hears_so() {
    let logs = CapturedLogs::start();
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [
            run_now(handed_over(
                "I've passed that to the chief.",
                "rerun the harold build",
            )),
            run_now(said("It was typed into the chief's pane.")),
        ],
    );
    h.script([
        text("ask the chief to rerun the harold build"),
        text("did the chief get it"),
    ]);
    h.set_chief(EffectiveAgentState::Busy);
    h.start().await;

    h.speak().await;
    sleep(Duration::from_secs(1)).await;

    assert_eq!(h.played(), ["I've passed that to the chief."]);
    assert_eq!(
        h.keys(),
        [
            Key::Text("%2".into(), "🎙 hand-over H1: rerun the harold build".into()),
            Key::Enter("%2".into()),
        ]
    );
    assert_eq!(h.status().last_result, Some(TurnResult::HandedOver));
    past_the_tail().await;
    h.speak().await;

    let second = answerer.log().asked[1].clone();
    assert_eq!(second.words, "did the chief get it");
    assert_eq!(
        second.call.hand_overs,
        [HandOverFact {
            id: "H1".into(),
            request: "rerun the harold build".into(),
            state: HandOverState::Typed,
        }]
    );
    assert_eq!(
        second.call.remarks,
        [
            Remark {
                who: Who::You,
                text: "ask the chief to rerun the harold build".into()
            },
            Remark {
                who: Who::Answerer,
                text: "I've passed that to the chief.".into()
            },
        ]
    );
    let text = logs.text();
    assert!(text.contains("hand-over typed hand_over_id=H1 pane_id=%2 typed_while_busy=true"));
    assert!(!text.contains("rerun the harold build"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_with_no_line_of_the_answerers_own_is_said_by_harold() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(handed_over("", "rerun it"))]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.played(), [PASSED_ON]);
    assert_eq!(h.keys().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_goes_on_trying_after_the_call_ends() {
    let logs = CapturedLogs::start();
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(handed_over("Passed.", "rerun it"))]);
    let held = "────\n❯ half a sentence\n────\n  -- INSERT --\n";
    h.ports.0.panes.set(|s| s.screen = Some(held.to_string()));
    h.start().await;
    h.speak().await;
    assert!(h.keys().is_empty());

    h.calls.stop().await;
    settle().await;
    assert_eq!(h.state(), CallState::Off);
    h.ports
        .0
        .panes
        .set(|s| s.screen = Some("────\n❯ \n────\n  -- INSERT --\n".to_string()));
    sleep(Duration::from_secs(6)).await;
    settle().await;

    assert_eq!(h.keys().len(), 2);
    assert!(logs.text().contains("hand-over typed hand_over_id=H1"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_run_over_three_seconds_plays_the_tick_every_three_seconds() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_after(7, said("Done."))]);
    h.start().await;

    h.speak().await;
    assert_eq!(h.played(), [] as [&str; 0]);
    sleep(Duration::from_millis(3100)).await;
    settle().await;
    assert_eq!(h.played(), ["tick"]);
    sleep(Duration::from_secs(3)).await;
    settle().await;
    assert_eq!(h.played(), ["tick", "tick"]);
    sleep(Duration::from_secs(2)).await;
    settle().await;

    assert_eq!(h.played(), ["tick", "tick", "Done."]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failed_run_sends_the_turn_to_the_chief_and_the_next_turn_tries_the_answerer_again() {
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [
            run_now(Answer::Failed(Failure::TimedOut)),
            run_now(said("Fine.")),
        ],
    );
    h.script([text("first words"), text("second words")]);
    h.start().await;

    h.speak().await;

    assert_eq!(h.keys(), typed("%2", "first words"));
    assert_eq!(h.status().last_result, Some(TurnResult::Typed));
    assert_eq!(h.state(), CallState::Waiting);
    // The chief answers, as today; its line is the answer, so nothing comes before it.
    h.answer("the chief's answer").await;
    assert_eq!(h.played(), ["the chief's answer"]);
    past_the_tail().await;
    h.speak().await;

    assert_eq!(answerer.log().asked.len(), 2);
    assert_eq!(answerer.log().asked[1].words, "second words");
    assert_eq!(h.keys().len(), 2);
    assert_eq!(h.played(), ["the chief's answer", "Fine."]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failed_turn_for_a_busy_chief_is_held_as_before() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(Answer::Failed(Failure::Exited))]);
    h.set_chief(EffectiveAgentState::Busy);
    h.start().await;

    h.speak().await;

    assert_eq!(h.played(), [CHIEF_BUSY]);
    assert!(h.keys().is_empty());
    h.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(2)).await;
    assert_eq!(h.keys(), typed("%2", "hello there"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_answerer_that_cannot_start_is_not_tried_again_in_the_call() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(Answer::Failed(Failure::NotStarted))]);
    h.script([text("first words"), text("second words")]);
    h.start().await;

    h.speak().await;
    h.answer("answer").await;
    h.speak().await;

    assert_eq!(answerer.log().asked.len(), 1);
    let mut want = typed("%2", "first words");
    want.extend(typed("%2", "second words"));
    assert_eq!(h.keys(), want);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_said_while_a_run_goes_waits_for_it_and_is_asked_with_the_first_in_the_history() {
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [
            run_after(5, said("First answer.")),
            run_now(said("Second answer.")),
        ],
    );
    h.script([text("first words"), text("second words")]);
    h.start().await;

    h.speak().await;
    past_the_tail().await;
    h.speak().await;
    settle().await;
    assert_eq!(answerer.log().asked.len(), 1);
    sleep(Duration::from_secs(6)).await;
    settle().await;
    past_the_tail().await;
    settle().await;

    let log = answerer.log();
    assert_eq!(log.most_going, 1, "two runs never overlap");
    assert_eq!(log.asked.len(), 2);
    assert_eq!(log.asked[1].words, "second words");
    assert_eq!(
        log.asked[1].call.remarks,
        [
            Remark {
                who: Who::You,
                text: "first words".into()
            },
            Remark {
                who: Who::Answerer,
                text: "First answer.".into()
            },
        ]
    );
    drop(log);
    let played = h.played();
    assert_eq!(played.last().map(String::as_str), Some("Second answer."));
    assert!(played.contains(&"First answer.".to_string()), "{played:?}");
    assert!(h.keys().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_while_a_run_goes_drops_its_line_but_not_its_hand_over() {
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [run_after(5, handed_over("Passed.", "rerun it"))],
    );
    h.start().await;
    h.speak().await;

    h.calls.talk_now();
    sleep(Duration::from_secs(6)).await;
    settle().await;

    assert!(
        !h.played().contains(&"Passed.".to_string()),
        "{:?}",
        h.played()
    );
    assert_eq!(h.keys().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_chiefs_line_between_turns_plays_after_from_the_chief_and_is_in_the_next_turns_history()
{
    let (h, answerer, _) = answering_harness();
    h.script([text("first words"), text("second words")]);
    h.start().await;
    h.speak().await;
    past_the_tail().await;

    let said_by_chief = h.say("H1: the build passed.", Speaker::Chief);
    settle().await;
    assert_eq!(said_by_chief.await.unwrap(), Some(true));
    settle().await;
    past_the_tail().await;
    h.speak().await;

    assert_eq!(
        h.played(),
        ["Fine.", FROM_THE_CHIEF, "H1: the build passed.", "Fine."]
    );
    let asked = answerer.log().asked.clone();
    assert!(asked[1].call.remarks.contains(&Remark {
        who: Who::Chief,
        text: "H1: the build passed.".into()
    }));
    assert_eq!(
        // After the ringback and the pick-up, and the answerer's first answer.
        h.sink().said.lock().unwrap()[3],
        Said::Line {
            text: FROM_THE_CHIEF.into(),
            by: Speaker::Chief
        }
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_drops_a_chief_line_waiting_behind_the_answerers_with_its_preface() {
    let (h, _, _) = answering_harness();
    h.start().await;
    h.sink().hold.store(true, Ordering::SeqCst);
    h.speak().await;
    settle().await;
    // The answerer's line is playing; the chief's line waits behind it.
    let said_by_chief = h.say("H1: done.", Speaker::Chief);
    settle().await;

    h.calls.talk_now();
    settle().await;

    assert_eq!(said_by_chief.await.unwrap(), Some(true));
    assert_eq!(h.played(), ["Fine."]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn without_the_answerer_a_chief_line_has_no_preface_and_turns_go_to_the_chief() {
    let h = Harness::new();
    h.start().await;
    h.speak().await;
    assert_eq!(h.keys(), typed("%2", "hello there"));

    h.answer("the chief's answer").await;
    let later = h.say("another line", Speaker::Chief);
    settle().await;
    later.await.unwrap();

    assert_eq!(h.played(), ["the chief's answer", "another line"]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_users_words_the_answerers_line_and_the_request_reach_no_log_and_no_export() {
    let logs = CapturedLogs::start();
    test_support::install();
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [run_now(handed_over(
            "SAY-CANARY-5521 passed.",
            "REQUEST-CANARY-5522",
        ))],
    );
    h.script([text("WORDS-CANARY-5520")]);
    h.start().await;

    h.speak().await;
    h.calls.stop().await;

    assert_eq!(
        h.keys()[0],
        Key::Text("%2".into(), "🎙 hand-over H1: REQUEST-CANARY-5522".into())
    );
    let text = logs.text();
    for canary in [
        "WORDS-CANARY-5520",
        "SAY-CANARY-5521",
        "REQUEST-CANARY-5522",
    ] {
        assert!(!text.contains(canary), "{canary}: {text}");
        assert!(!test_support::exported_text().contains(canary), "{canary}");
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_bye_while_a_run_goes_still_hands_over_but_says_nothing_more() {
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [run_after(2, handed_over("Passed.", "rerun it"))],
    );
    h.script([text("ask the chief to rerun it"), text("bye")]);
    h.start().await;
    h.speak().await;
    past_the_tail().await;

    h.speak().await;
    sleep(Duration::from_secs(3)).await;
    settle().await;

    assert_eq!(h.state(), CallState::Off);
    assert!(
        !h.played().contains(&"Passed.".to_string()),
        "{:?}",
        h.played()
    );
    // The hand-over still reached the chief, though the call ended before the run did.
    assert_eq!(h.keys().len(), 2, "{:?}", h.keys());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_run_that_ends_while_the_call_is_closing_down_still_hands_over() {
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [run_after(3, handed_over("Passed.", "rerun it"))],
    );
    h.worker().close_delay = Duration::from_secs(5);
    h.script([text("ask the chief to rerun it"), text("bye")]);
    h.start().await;
    h.speak().await;
    past_the_tail().await;

    h.speak().await;
    // Other events wait in the inbox ahead of the run's answer while the worker closes.
    sleep(Duration::from_secs(1)).await;
    h.calls.talk_now();
    drop(h.say("a late chief line", Speaker::Chief));
    sleep(Duration::from_secs(10)).await;
    settle().await;

    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.keys().len(), 2, "{:?}", h.keys());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hand_overs_after_the_call_ends_keep_counting_on() {
    let (h, answerer, _) = answering_harness();
    script_runs(
        &answerer,
        [
            run_now(handed_over("Passed.", "first request")),
            run_after(3, handed_over("Passed.", "second request")),
        ],
    );
    h.script([text("one"), text("two")]);
    h.start().await;
    h.speak().await;
    past_the_tail().await;
    h.speak().await;
    h.calls.stop().await;
    sleep(Duration::from_secs(5)).await;
    settle().await;

    let keys = format!("{:?}", h.keys());
    assert!(keys.contains("hand-over H1: first request"), "{keys}");
    assert!(keys.contains("hand-over H2: second request"), "{keys}");
}

// ---------------------------------------------------------------------------
// The ringing, the warm-up, and the speech models
// ---------------------------------------------------------------------------

use super::super::models::{Missing, Status as ModelsStatus};
use super::super::phrases::MODEL_MISSING;
use super::super::worker::{LoadCause, LoadFailure};
use crate::settings::{KOKORO_REVISION, WHISPER_REVISION};

fn missing_model(reason: &'static str) -> Missing {
    Missing {
        model: super::super::models::WHISPER.to_string(),
        reason,
        command: Some(format!(
            "`/venv/bin/python -c \"snapshot_download('{}', revision='{WHISPER_REVISION}')\"`",
            super::super::models::WHISPER
        )),
    }
}

fn load_refused(cause: LoadCause, model: Option<&'static str>) -> Health {
    Health::Gone(WorkerError::LoadRefused(LoadFailure { cause, model }))
}

/// Lets the clock run `seconds` more and the call's tasks with it.
async fn run_for(seconds: f64) {
    sleep(Duration::from_secs_f64(seconds)).await;
    settle().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_whose_worker_is_loading_rings_a_whole_cycle_every_3_s() {
    let h = Harness::new();
    h.worker().starts_loading = true;

    let status = h.start_ringing().await;
    settle().await;

    assert_eq!(status.state, CallState::Connecting);
    assert_eq!(h.played_all(), ["ringback"]);
    run_for(2.9).await;
    assert_eq!(h.played_all(), ["ringback"]);
    run_for(0.2).await;
    assert_eq!(h.played_all(), ["ringback", "ringback"]);
    run_for(3.0).await;
    let at = h.sink().played.lock().unwrap().clone();
    assert_eq!(at.len(), 3);
    assert_eq!(at[1].0 - at[0].0, Duration::from_secs(3));
    assert_eq!(at[2].0 - at[1].0, Duration::from_secs(3));
    assert_eq!(h.state(), CallState::Connecting);
    // The sound path names the ringback as it names the tick: a sound with no words.
    assert!(
        h.sink()
            .said
            .lock()
            .unwrap()
            .iter()
            .all(|said| *said == Said::Tick)
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_ready_mid_cycle_waits_for_the_cycle_to_end_then_the_call_picks_up_and_listens() {
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.start_ringing().await;
    run_for(1.6).await;

    h.set_health(Health::Ready);
    settle().await;

    // The ring is never cut short.
    assert_eq!(h.state(), CallState::Connecting);
    assert_eq!(h.played_all(), ["ringback"]);
    run_for(1.5).await;
    assert_eq!(h.played_all(), ["ringback", "pickup"]);
    assert_eq!(h.state(), CallState::Listening);
    run_for(10.0).await;
    assert_eq!(h.played_all(), ["ringback", "pickup"], "no ring follows");
    assert_eq!(h.state(), CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_loaded_worker_rings_exactly_one_cycle_plays_the_pick_up_and_then_listens() {
    let h = Harness::new();
    h.sink().hold.store(true, Ordering::SeqCst);

    let status = h.start_ringing().await;
    settle().await;

    assert_eq!(status.state, CallState::Connecting);
    assert_eq!(h.played_all(), ["ringback"]);
    // The ringback plays for its 3 s.
    run_for(3.0).await;
    h.sink().release();
    run_for(0.05).await;
    assert_eq!(h.played_all(), ["ringback", "pickup"]);
    assert_eq!(h.state(), CallState::Connecting, "until the pick-up ends");
    run_for(0.15).await;
    h.sink().release();
    settle().await;
    assert_eq!(h.state(), CallState::Listening);
    run_for(10.0).await;
    assert_eq!(h.played_all(), ["ringback", "pickup"]);
    assert_eq!(h.ports.state().opened, 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_becomes_ready_4_s_in_rings_two_cycles() {
    test_support::install();
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.start_ringing().await;
    run_for(4.0).await;

    h.set_health(Health::Ready);
    run_for(2.1).await;

    assert_eq!(h.played_all(), ["ringback", "ringback", "pickup"]);
    assert_eq!(h.state(), CallState::Listening);
    // Spans of calls in other tests are in the same list: find this call's by what it holds.
    let spans = test_support::spans();
    assert!(
        spans.iter().any(|span| {
            let attr = |key| test_support::span_attr(span, key);
            span.name == "harold.call_ring"
                && attr("cycles").as_deref() == Some("2")
                && attr("worker").as_deref() == Some("loaded_now")
        }),
        "a call that loaded its worker rang two cycles"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_whose_worker_is_ready_logs_one_cycle_and_the_warm_up() {
    test_support::install();
    let (h, answerer, _) = answering_harness();
    answerer.log().warm_takes = Duration::from_millis(1200);

    h.start().await;

    // Spans of calls in other tests are in the same list: find this call's by its warm-up.
    let spans = test_support::spans();
    assert!(
        spans.iter().any(|span| {
            let attr = |key| test_support::span_attr(span, key);
            span.name == "harold.call_ring"
                && attr("cycles").as_deref() == Some("1")
                && attr("worker").as_deref() == Some("ready")
                && attr("warm_ms").as_deref() == Some("1200")
        }),
        "a call whose worker was ready rang one cycle"
    );
    let sources: Vec<String> = spans
        .iter()
        .filter(|span| span.name == "harold.call_play")
        .filter_map(|span| test_support::span_attr(span, "source"))
        .collect();
    assert!(sources.contains(&"ringback".to_string()));
    assert!(sources.contains(&"pickup".to_string()));
    assert!(!sources.contains(&"connecting".to_string()));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_cold_worker_is_started_during_the_ringing() {
    let h = Harness::new();
    h.worker().starts_loading = true;

    h.start_ringing().await;
    settle().await;

    assert_eq!(h.worker().starts, 1);
    assert_eq!(h.state(), CallState::Connecting);
    assert_eq!(h.played_all(), ["ringback"]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn speech_during_the_ringing_starts_no_turn() {
    let h = Harness::new();
    h.script([text("hello")]);
    h.start_ringing().await;
    // Past the 0.3 s after the ringback began, in the quiet of the cycle.
    run_for(1.0).await;
    h.feed(quiet(500));
    settle().await;

    h.speak().await;
    run_for(2.5).await;

    assert_eq!(h.played_all(), ["ringback", "pickup"]);
    assert!(h.worker().transcribed.is_empty());
    assert_eq!(h.status().last_result, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_ringing_call_is_not_idle() {
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.start_ringing().await;

    run_for(11.0 * 60.0).await;

    assert_eq!(h.state(), CallState::Connecting);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_loaded_worker_records_no_load_wait() {
    test_support::install();
    let (h, answerer, _) = answering_harness();
    h.script([text("hello")]);
    h.start().await;

    h.speak().await;

    let trace = answerer.log().asked[0].trace;
    let spans = test_support::spans();
    let transcribe = spans
        .iter()
        .find(|span| span.name == "harold.call_transcribe" && span.span_context.trace_id() == trace)
        .expect("the turn was transcribed");
    assert_eq!(
        test_support::span_attr(transcribe, "load_waited_ms").as_deref(),
        Some("0")
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_answered_turn_plays_no_phrase_before_the_answer() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(said("Fine."))]);
    h.start().await;
    h.sink().hold.store(true, Ordering::SeqCst);

    h.speak().await;

    assert_eq!(h.played(), ["Fine."]);
    assert_eq!(h.state(), CallState::Speaking);
    h.sink().release();
    settle().await;
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(h.worker().said, ["Fine."], "nothing else was made");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_that_ends_the_call_or_is_not_heard_plays_only_its_own_phrase() {
    let (h, _, _) = answering_harness();
    h.script([Heard::Unlikely("mumble".into()), text("bye")]);
    h.start().await;

    h.speak().await;
    assert_eq!(h.played(), [NOTHING_HEARD]);
    past_the_tail().await;
    h.speak().await;

    assert_eq!(h.played(), [NOTHING_HEARD, BYE]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_phrase_is_made_at_the_start_of_a_call() {
    let (h, _, _) = answering_harness();

    h.start().await;
    settle().await;

    assert!(h.worker().said.is_empty());
    assert!(h.played().is_empty(), "the ringing is not a phrase");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_warm_up_runs_once_and_the_first_turn_within_30_s_uses_its_facts() {
    let (h, answerer, _) = answering_harness();
    script_runs(&answerer, [run_now(said("One.")), run_now(said("Two."))]);
    h.script([text("first"), text("second")]);
    h.start_ringing().await;
    run_for(3.5).await;
    h.feed(quiet(500));
    settle().await;

    h.speak().await;
    past_the_tail().await;
    h.speak().await;

    let log = answerer.log();
    assert_eq!(log.warms, 1);
    assert_eq!(log.asked.len(), 2);
    assert_eq!(log.asked[0].facts, Some(serde_json::json!({"warm": 1})));
    assert_eq!(log.asked[1].facts, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn facts_older_than_30_s_are_read_again() {
    let (h, answerer, _) = answering_harness();
    h.script([text("late")]);
    h.start().await;

    run_for(31.0).await;
    h.speak().await;

    assert_eq!(answerer.log().asked[0].facts, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_warm_up_that_hangs_is_cut_at_5_s_and_the_call_picks_up() {
    let (h, answerer, _) = answering_harness();
    answerer.log().warm_hangs = true;
    h.start_ringing().await;

    // At 3 s the call is not ready: another whole cycle plays.
    run_for(3.1).await;
    assert_eq!(h.played_all(), ["ringback", "ringback"]);
    assert_eq!(h.state(), CallState::Connecting);
    // The warm-up is cut at 5 s, and the cycle that ends at 6 s picks up.
    run_for(3.0).await;

    assert_eq!(h.played_all(), ["ringback", "ringback", "pickup"]);
    assert_eq!(h.state(), CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_worker_starts_when_the_speech_models_are_on_the_mac_and_the_screen_follows_the_download()
 {
    let h = Harness::new();
    let models = SpeechModels::stubbed(ModelsStatus::Checking);
    h.ports.state().models = Some(Arc::clone(&models));

    let status = h.start_ringing().await;
    settle().await;

    assert_eq!(status.state, CallState::Connecting);
    assert_eq!(models.asked(), ["for_call"]);
    assert_eq!(
        h.worker().starts,
        0,
        "no worker before the models are there"
    );
    assert_eq!(h.played_all(), ["ringback"]);
    assert_eq!(h.status().model_percent, None);
    models.set_status(ModelsStatus::Downloading {
        model: super::super::models::WHISPER.into(),
        bytes: 450,
        total: 1000,
    });
    settle().await;
    assert_eq!(h.status().model_percent, Some(45));
    run_for(3.2).await;
    assert_eq!(
        h.played_all(),
        ["ringback", "ringback"],
        "the ring goes on during the download"
    );

    models.set_status(ModelsStatus::Ready);
    settle().await;

    assert_eq!(h.worker().starts, 1);
    // The ring ends with its cycle.
    assert_eq!(h.state(), CallState::Connecting);
    run_for(3.0).await;
    assert_eq!(h.state(), CallState::Listening);
    assert_eq!(h.status().model_percent, None);
    run_for(5.0).await;
    assert_eq!(h.played_all(), ["ringback", "ringback", "pickup"]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn models_that_are_ready_start_the_worker_at_once() {
    let h = Harness::new();
    let models = SpeechModels::stubbed(ModelsStatus::Ready);
    h.ports.state().models = Some(models);

    let status = h.start().await;

    assert_eq!(status.state, CallState::Listening);
    assert_eq!(h.worker().starts, 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failed_download_stops_the_tone_says_the_model_is_missing_and_ends_the_call() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    let models = SpeechModels::stubbed(ModelsStatus::Checking);
    h.ports.state().models = Some(Arc::clone(&models));
    h.start_ringing().await;
    settle().await;
    assert_eq!(h.played_all(), ["ringback"]);

    models.set_status(ModelsStatus::Failed(missing_model("network")));
    settle().await;

    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.tts(), [MODEL_MISSING]);
    assert_eq!(h.ports.state().ended, ["voice_failed"]);
    let missing = h.ports.state().missing.clone();
    assert_eq!(missing, [missing_model("network")]);
    assert_eq!(h.worker().starts, 0);
    sleep(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(h.played_all(), ["ringback"], "the ring stopped");
    assert!(logs.text().contains("call ended reason=\"voice_failed\""));
    assert!(h.call_dirs().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_tapped_after_a_failure_asks_the_models_to_try_again_and_one_that_cannot_ends_at_once()
 {
    let h = Harness::new();
    let models = SpeechModels::stubbed(ModelsStatus::Failed(missing_model("disk_full")));
    h.ports.state().models = Some(Arc::clone(&models));

    h.start().await;
    settle().await;

    // The call asked the models first; a failure that cannot be retried ends it now.
    assert_eq!(models.asked(), ["for_call"]);
    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.tts(), [MODEL_MISSING]);
    assert_eq!(h.ports.state().missing[0].reason, "disk_full");
    assert_eq!(h.worker().starts, 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_worker_that_cannot_find_a_model_has_the_models_look_again_and_starts_once_more() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    let models = SpeechModels::stubbed(ModelsStatus::Ready);
    h.ports.state().models = Some(Arc::clone(&models));
    h.worker().starts_loading = true;
    h.start().await;
    assert_eq!(h.worker().starts, 1);

    h.set_health(load_refused(
        LoadCause::ModelMissing,
        Some(super::super::models::WHISPER),
    ));
    settle().await;

    assert_eq!(models.asked(), ["for_call", "model_missing"]);
    assert_eq!(h.state(), CallState::Connecting);
    assert_eq!(h.worker().starts, 1, "waits for the models");
    let text = logs.text();
    assert!(
        text.contains("voice worker exited reason=\"failed_to_load\" cause=\"model_missing\" model=\"mlx-community/whisper-large-v3-turbo\""),
        "{text}"
    );

    models.set_status(ModelsStatus::Ready);
    settle().await;
    assert_eq!(h.worker().starts, 2);
    h.set_health(Health::Ready);
    // The call rings a whole cycle again before it listens.
    run_for(3.2).await;
    assert_eq!(h.state(), CallState::Listening);

    // Not again: a second time ends the call with the notice.
    h.worker().starts_loading = false;
    h.set_health(load_refused(
        LoadCause::ModelMissing,
        Some(super::super::models::WHISPER),
    ));
    settle().await;
    assert_eq!(h.state(), CallState::Off);
    assert_eq!(h.tts(), [MODEL_MISSING]);
    assert_eq!(
        h.ports.state().missing[0].model,
        super::super::models::WHISPER
    );
    assert_eq!(h.ports.state().missing[0].reason, "failed");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn any_other_cause_of_a_failed_load_says_the_voice_failed() {
    for cause in [
        LoadCause::ImportFailed,
        LoadCause::VoiceMissing,
        LoadCause::LoadFailed,
    ] {
        let logs = CapturedLogs::start();
        let h = Harness::new();
        let models = SpeechModels::stubbed(ModelsStatus::Ready);
        h.ports.state().models = Some(Arc::clone(&models));
        h.worker().starts_loading = true;
        h.start().await;

        h.set_health(load_refused(cause, Some(super::super::models::KOKORO)));
        settle().await;

        assert_eq!(h.state(), CallState::Off, "{cause:?}");
        assert_eq!(h.tts(), [VOICE_FAILED], "{cause:?}");
        assert_eq!(models.asked(), ["for_call"], "{cause:?}");
        assert!(h.ports.state().missing.is_empty());
        let text = logs.text();
        assert!(
            text.contains(&format!("cause=\"{}\"", cause.as_str())),
            "{text}"
        );
        // The model is named only when the model is what is missing.
        assert!(!text.contains("model=\""), "{text}");
    }
    let _ = KOKORO_REVISION;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_logs_the_workers_load_when_it_started_the_worker() {
    let logs = CapturedLogs::start();
    let h = Harness::new();
    h.worker().starts_loading = true;
    h.start().await;
    h.set_health(Health::Ready);
    settle().await;

    h.calls.stop().await;
    settle().await;

    let text = logs.text();
    assert!(text.contains("voice worker ready"), "{text}");
    assert!(text.contains("started_by=\"call\""), "{text}");
    assert!(
        text.contains("voice worker stopped reason=\"call_ended\""),
        "{text}"
    );
}
