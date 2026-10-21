//! More of a call on the fakes of `voice_input_tests.rs`: the worker's health as it changes,
//! a sound the sink cannot play, a turn said before a bye, a second START, and the fixes of
//! the final review: the idle end, the mid-call mute end and the talk-now hold's limit.

use super::super::phrases::phrase_file;
use super::super::wav::wav_bytes;
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
    h.feed(loud(1500));
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

    let spans = trace_of_clip(1500);
    let turn = root(&spans);
    assert_eq!(attr(turn, "pane_id").as_deref(), Some(pane.as_str()));
    assert!(attr(turn, "answer_ms").is_some());
}
