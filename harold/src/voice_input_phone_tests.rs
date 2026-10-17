//! The phone's side of a call, on paused time: pieces posted into the open call's source,
//! sounds offered until the phone reports them played, and lines for the phone's own voice.

use super::phone::{PhonePorts, PhoneSink};
use super::phrases::{NO_ANSWER_IN_THREAD, VOICE_STOPPED};
use super::tests::{FakePorts, UPDATE, settle};
use super::wav::wav_bytes;
use super::{
    ANSWER_KEEP, AudioRefused, AudioSink, AudioSource, CallPorts, CallState, PHONE_SILENT_LIMIT,
    PIECE_LIMIT_BYTES, PIECES_QUEUED, PLAY_GRACE, PhoneLine, PhoneSound, PhoneSpeech, PhoneView,
    SPEECH_BASE, SPEECH_PER_CHAR, Said, Speaker,
};
use crate::outbound::fake::CapturedLogs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout};

/// A folder for the test's WAV files, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("harold-phone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// A 16 kHz mono WAV whose header says it lasts `secs`; the samples themselves are left
    /// out, since only the header gives a sound its length.
    fn wav(&self, name: &str, secs: u32) -> PathBuf {
        let mut bytes = wav_bytes(&[], 16_000);
        bytes[40..44].copy_from_slice(&(secs * 32_000).to_le_bytes());
        let path = self.0.join(format!("{name}.wav"));
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn play(sink: &Arc<PhoneSink>, wav: &Path, said: Said) -> JoinHandle<Result<(), String>> {
    let sink = Arc::clone(sink);
    let wav = wav.to_path_buf();
    tokio::spawn(async move { sink.play_said(&wav, &said).await })
}

/// The sound the phone is offered now.
fn offered(line: &PhoneLine) -> PhoneSound {
    line.view()
        .borrow()
        .sound
        .clone()
        .expect("a sound is offered")
}

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_piece_is_read_as_samples_stamped_by_when_it_was_recorded() {
    let line = PhoneLine::new(true);
    let opened = Instant::now();
    let mut source = line.open_source();
    sleep(Duration::from_secs(1)).await;

    line.audio(250, &[0x01, 0x00, 0xff, 0x7f]).unwrap();

    assert_eq!(
        source.next_stamped().await,
        Some((vec![1, i16::MAX], opened + Duration::from_millis(250)))
    );
    line.audio(500, &[0x02, 0x00]).unwrap();
    assert_eq!(source.next_block().await, Some(vec![2]));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn pieces_are_refused_with_no_call_odd_too_large_or_past_64_queued() {
    let line = PhoneLine::new(true);
    assert_eq!(line.audio(0, &[0; 8_000]), Err(AudioRefused::NoCall));

    let source = line.open_source();
    assert_eq!(line.audio(0, &[0; 3]), Err(AudioRefused::Malformed));
    assert_eq!(
        line.audio(0, &vec![0; PIECE_LIMIT_BYTES + 2]),
        Err(AudioRefused::TooLarge)
    );
    assert_eq!(line.audio(0, &vec![0; PIECE_LIMIT_BYTES]), Ok(()));
    assert_eq!(line.audio(0, &[]), Ok(()));
    for _ in 2..PIECES_QUEUED {
        assert_eq!(line.audio(0, &[0; 8_000]), Ok(()));
    }
    assert_eq!(line.audio(0, &[0; 8_000]), Err(AudioRefused::Full));

    // The call's source is gone: what arrives late is not kept for the next call.
    drop(source);
    assert_eq!(line.audio(0, &[0; 8_000]), Err(AudioRefused::NoCall));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_new_source_ends_the_old_one() {
    let line = PhoneLine::new(true);
    let mut old = line.open_source();
    let mut new = line.open_source();

    line.audio(0, &[0x05, 0x00]).unwrap();

    assert_eq!(old.next_stamped().await, None);
    assert_eq!(new.next_block().await, Some(vec![5]));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_source_ends_15_s_after_the_last_piece_while_nothing_plays() {
    let line = PhoneLine::new(true);
    let mut source = line.open_source();
    sleep(Duration::from_secs(5)).await;
    line.audio(5_000, &[0, 0]).unwrap();
    let piece_at = Instant::now();

    assert!(source.next_stamped().await.is_some());
    let early = timeout(
        PHONE_SILENT_LIMIT - Duration::from_millis(1),
        source.next_stamped(),
    )
    .await;
    assert!(early.is_err(), "the source ended before 15 s");
    assert_eq!(source.next_stamped().await, None);
    assert_eq!(Instant::now(), piece_at + PHONE_SILENT_LIMIT);

    assert_eq!(source.next_stamped().await, None);
    assert_eq!(line.audio(20_000, &[0, 0]), Err(AudioRefused::NoCall));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_source_ends_15_s_after_the_last_piece_while_offered_sounds_go_unfetched() {
    let scratch = Scratch::new();
    let tick = scratch.wav("tick", 0);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let start = Instant::now();
    let mut source = line.open_source();
    line.audio(0, &[0, 0]).unwrap();
    assert!(source.next_stamped().await.is_some());

    // The chief is thinking, so a tick is offered every 3 s; the phone is gone and fetches none.
    let ticking = tokio::spawn(async move {
        loop {
            let _ = sink.play_said(&tick, &Said::Tick).await;
            sleep(Duration::from_secs(3)).await;
        }
    });

    let ended = timeout(Duration::from_secs(60), source.next_stamped()).await;
    assert_eq!(ended, Ok(None), "the source did not end within 60 s");
    assert_eq!(Instant::now(), start + PHONE_SILENT_LIMIT);
    ticking.abort();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_source_waits_through_a_30_s_answer_with_no_pieces() {
    let scratch = Scratch::new();
    let wav = scratch.wav("answer", 30);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let start = Instant::now();
    let mut source = line.open_source();
    let reading = tokio::spawn(async move { source.next_stamped().await });

    let said = Said::Line {
        text: "Done.".into(),
        by: Speaker::Chief,
    };
    let playing = play(&sink, &wav, said);
    settle().await;
    let sound = offered(&line);
    assert!(line.answer(&sound.id).is_some());
    sleep_until(start + Duration::from_secs(30)).await;
    line.played(&sound.id);
    assert_eq!(playing.await.unwrap(), Ok(()));

    sleep_until(start + Duration::from_secs(40)).await;
    assert!(!reading.is_finished(), "the source ended during the answer");
    line.audio(40_000, &[0x02, 0x00]).unwrap();
    assert_eq!(
        reading.await.unwrap(),
        Some((vec![2], start + Duration::from_secs(40)))
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_source_waits_through_a_line_for_the_phones_voice() {
    let line = PhoneLine::new(true);
    let ports = Arc::new(PhonePorts::new(FakePorts::new(), Arc::clone(&line)));
    let start = Instant::now();
    let mut source = ports.open_source().unwrap();
    let reading = tokio::spawn(async move { source.next_stamped().await });

    sleep(Duration::from_secs(2)).await;
    let speaking = tokio::spawn({
        let ports = Arc::clone(&ports);
        async move { ports.speak_with_tts("x".repeat(300).as_str()).await }
    });
    sleep(Duration::from_secs(18)).await;
    let speech = line.view().borrow().speech.clone().expect("a line waits");
    line.played(&speech.id);
    speaking.await.unwrap();

    // The 15 s run from the line's end at 20 s.
    sleep_until(start + Duration::from_secs(34)).await;
    assert!(!reading.is_finished(), "the source ended during the line");
    assert_eq!(reading.await.unwrap(), None);
    assert_eq!(Instant::now(), start + Duration::from_secs(35));
}

// ---------------------------------------------------------------------------
// The sink
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_sound_is_offered_until_the_phone_reports_it_played() {
    let scratch = Scratch::new();
    let wav = scratch.wav("bye", 1);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));

    let playing = play(&sink, &wav, Said::Phrase("Bye.".into()));
    settle().await;

    let sound = offered(&line);
    assert_eq!(sound.said, Said::Phrase("Bye.".into()));
    assert_eq!(sound.id.len(), 32);
    assert_eq!(line.answer(&sound.id), Some(std::fs::read(&wav).unwrap()));
    assert!(!playing.is_finished());

    line.played(&sound.id);

    assert_eq!(playing.await.unwrap(), Ok(()));
    assert_eq!(line.view().borrow().sound, None);
    assert_eq!(line.answer(&sound.id), None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stop_ends_the_sound_at_once_and_takes_back_the_offer() {
    let scratch = Scratch::new();
    let wav = scratch.wav("answer", 20);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let playing = play(&sink, &wav, Said::Tick);
    settle().await;
    let sound = offered(&line);
    let stopped_at = Instant::now();

    sink.stop();

    assert_eq!(playing.await.unwrap(), Ok(()));
    assert_eq!(Instant::now(), stopped_at);
    assert_eq!(line.view().borrow().sound, None);
    assert_eq!(line.answer(&sound.id), None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_fetched_sound_with_no_report_counts_as_played_after_its_length_and_10_s() {
    let scratch = Scratch::new();
    let wav = scratch.wav("answer", 2);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let start = Instant::now();
    let playing = play(&sink, &wav, Said::Tick);
    settle().await;
    let sound = offered(&line);
    assert!(line.answer(&sound.id).is_some());

    sleep(Duration::from_millis(11_900)).await;
    assert!(!playing.is_finished(), "the sound ended before 12 s");

    assert_eq!(playing.await.unwrap(), Ok(()));
    assert_eq!(Instant::now(), start + Duration::from_secs(2) + PLAY_GRACE);
    assert_eq!(line.view().borrow().sound, None);
    assert_eq!(line.answer(&sound.id), None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_sound_never_fetched_fails_after_its_length_and_10_s() {
    let scratch = Scratch::new();
    let wav = scratch.wav("answer", 2);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let start = Instant::now();

    let played = sink.play_said(&wav, &Said::Tick).await;

    assert_eq!(played, Err("not fetched by the phone".to_string()));
    assert_eq!(Instant::now(), start + Duration::from_secs(12));
    assert_eq!(line.view().borrow().sound, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_sounds_length_is_read_past_the_chunks_before_its_data() {
    let scratch = Scratch::new();
    // A `LIST` chunk of 3 bytes, padded to 4, between `fmt ` and a 2 s `data` chunk.
    let mut bytes = wav_bytes(&[], 16_000);
    let data = bytes.split_off(36);
    bytes.extend_from_slice(b"LIST");
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(b"abc\0");
    bytes.extend_from_slice(&data[..4]);
    bytes.extend_from_slice(&64_000u32.to_le_bytes());
    let wav = scratch.0.join("listed.wav");
    std::fs::write(&wav, bytes).unwrap();
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let start = Instant::now();
    let playing = play(&sink, &wav, Said::Tick);
    settle().await;
    assert!(line.answer(&offered(&line).id).is_some());

    assert_eq!(playing.await.unwrap(), Ok(()));
    assert_eq!(Instant::now(), start + Duration::from_secs(2) + PLAY_GRACE);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_file_that_cannot_be_read_waits_only_the_grace() {
    let scratch = Scratch::new();
    let line = PhoneLine::new(true);
    let sink = PhoneSink::new(Arc::clone(&line));
    let start = Instant::now();

    let played = sink.play(&scratch.0.join("missing.wav")).await;

    assert!(played.is_err());
    assert_eq!(Instant::now(), start + PLAY_GRACE);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_sound_is_offered_longer_than_2_minutes() {
    let scratch = Scratch::new();
    let wav = scratch.wav("answer", 200);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let start = Instant::now();
    let playing = play(&sink, &wav, Said::Tick);
    settle().await;
    let sound = offered(&line);
    assert!(line.answer(&sound.id).is_some());

    assert_eq!(playing.await.unwrap(), Ok(()));

    assert_eq!(Instant::now(), start + ANSWER_KEEP);
    assert_eq!(line.answer(&sound.id), None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_play_dropped_midway_takes_back_its_offer() {
    let scratch = Scratch::new();
    let wav = scratch.wav("answer", 20);
    let line = PhoneLine::new(true);
    let sink = Arc::new(PhoneSink::new(Arc::clone(&line)));
    let playing = play(&sink, &wav, Said::Tick);
    settle().await;
    let sound = offered(&line);

    playing.abort();
    let _ = playing.await;

    assert_eq!(line.view().borrow().sound, None);
    assert_eq!(line.answer(&sound.id), None);
}

// ---------------------------------------------------------------------------
// The ports
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_line_for_the_phones_voice_waits_for_its_report_or_its_limit() {
    let base = FakePorts::new();
    let line = PhoneLine::new(true);
    let ports = Arc::new(PhonePorts::new(base.clone(), Arc::clone(&line)));
    let start = Instant::now();

    let speaking = tokio::spawn({
        let ports = Arc::clone(&ports);
        async move { ports.speak_with_tts(VOICE_STOPPED).await }
    });
    settle().await;
    let speech = line.view().borrow().speech.clone().expect("a line waits");
    assert_eq!(
        speech,
        PhoneSpeech {
            id: speech.id.clone(),
            text: VOICE_STOPPED.to_string(),
        }
    );
    assert_eq!(speech.id.len(), 32);
    line.played(&speech.id);
    speaking.await.unwrap();
    assert_eq!(Instant::now(), start);
    assert_eq!(line.view().borrow().speech, None);

    ports.speak_with_tts(VOICE_STOPPED).await;
    assert_eq!(VOICE_STOPPED.chars().count(), 30);
    assert_eq!(Instant::now(), start + SPEECH_BASE + SPEECH_PER_CHAR * 30);
    assert_eq!(Instant::now(), start + Duration::from_millis(4_400));
    assert_eq!(line.view().borrow().speech, None);

    let speaking = tokio::spawn({
        let ports = Arc::clone(&ports);
        async move { ports.speak_with_tts(VOICE_STOPPED).await }
    });
    settle().await;
    let stopped_at = Instant::now();
    ports.stop_desk_line();
    speaking.await.unwrap();
    assert_eq!(Instant::now(), stopped_at);
    assert_eq!(line.view().borrow().speech, None);

    assert!(base.state().tts.is_empty());
    assert_eq!(base.state().desk_stops, 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_phone_ports_speak_and_end_a_call_on_the_phone() {
    let logs = CapturedLogs::start();
    let base = FakePorts::new();
    base.state().muted = true;
    let line = PhoneLine::new(true);
    let ports = PhonePorts::new(base.clone(), Arc::clone(&line));

    assert!(!ports.voice_muted());
    assert!(ports.keep_awake().is_ok());
    assert_eq!(ports.no_answer_phrase(), NO_ANSWER_IN_THREAD);
    assert!(Arc::ptr_eq(&ports.sink(), &ports.sink()));
    assert_eq!(ports.calls_dir(), base.calls_dir());
    assert_eq!(ports.phrases_dir(), base.phrases_dir());
    assert_eq!(ports.voice_name(), base.voice_name());
    assert_eq!(ports.chief_folder(), base.chief_folder());
    assert_eq!(ports.turn_end_update("fix it").await, UPDATE);
    assert_eq!(base.state().update_prompts, ["fix it"]);

    ports.call_ended("stopped");

    assert_eq!(
        *line.view().borrow(),
        PhoneView {
            state: CallState::Off,
            transcript: None,
            sound: None,
            speech: None,
            ended: Some("stopped"),
        }
    );
    assert!(logs.text().contains("app call ended reason=\"stopped\""));
}

#[test]
fn a_line_starts_off_or_not_set_up() {
    let view = PhoneLine::new(true).view().borrow().clone();
    assert_eq!(view.state, CallState::Off);
    assert_eq!(view.transcript, None);
    assert_eq!(view.ended, None);
    assert_eq!(view.sound, None);
    assert_eq!(view.speech, None);
    assert_eq!(
        PhoneLine::new(false).view().borrow().state,
        CallState::NotSetUp
    );
}
