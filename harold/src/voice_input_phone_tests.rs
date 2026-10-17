//! The phone's side of a call, on paused time: pieces posted into the open call's source,
//! sounds offered until the phone reports them played, and lines for the phone's own voice.

use super::phone::{PhonePorts, PhoneSink, STATUS_EVERY};
use super::phrases::{
    BYE, CHIEF_BUSY, NO_ANSWER_IN_THREAD, NO_CHIEF, NOTHING_HEARD, VOICE_STOPPED, phrase_file,
};
use super::tests::{FakePorts, Heard, UPDATE, loud, past_the_tail, quiet, settle, text, typed};
use super::wav::wav_bytes;
use super::{
    ANSWER_KEEP, AudioRefused, AudioSink, AudioSource, CallPorts, CallState, CallStatus,
    PHONE_SILENT_LIMIT, PIECE_LIMIT_BYTES, PIECES_QUEUED, PLAY_GRACE, PhoneCalls, PhoneLine,
    PhoneSound, PhoneSpeech, PhoneView, SPEECH_BASE, SPEECH_PER_CHAR, Said, Speaker, TurnResult,
};
use crate::agent::domain::EffectiveAgentState;
use crate::outbound::fake::CapturedLogs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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

// ---------------------------------------------------------------------------
// A call from the phone
// ---------------------------------------------------------------------------

/// Samples in one posted piece: a quarter of a second at 16 kHz.
const PIECE_SAMPLES: usize = 4_000;
const PIECE_LENGTH: Duration = Duration::from_millis(250);

/// A phone calling through `PhoneCalls` on fake ports: it posts audio, and plays the sounds
/// and says the lines it is offered.
struct Phone {
    base: FakePorts,
    calls: Arc<PhoneCalls<FakePorts>>,
    said: Arc<Mutex<Vec<Said>>>,
    spoken: Arc<Mutex<Vec<String>>>,
}

impl Phone {
    fn new() -> Self {
        let base = FakePorts::new();
        let calls = Arc::new(PhoneCalls::new(base.clone(), true));
        Self {
            base,
            calls,
            said: Arc::default(),
            spoken: Arc::default(),
        }
    }

    /// Posts `samples` as recorded from `at`, in quarter-second pieces stamped from the base
    /// of the open call's source.
    fn post(&self, at: Instant, samples: Vec<i16>) {
        let base = self.calls.source_base().expect("a call is open");
        let from = at.duration_since(base);
        for (n, piece) in samples.chunks(PIECE_SAMPLES).enumerate() {
            let at = from + PIECE_LENGTH * u32::try_from(n).unwrap();
            let at_ms = u64::try_from(at.as_millis()).unwrap();
            let bytes: Vec<u8> = piece
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect();
            self.calls
                .audio(at_ms, &bytes)
                .expect("the call takes the piece");
        }
    }

    /// From now on, fetches each sound the phone is offered, plays it for `length` and
    /// reports it played; and says each line for its own voice for `length`.
    fn auto_play(&self, length: Duration) {
        let calls = Arc::clone(&self.calls);
        let said = Arc::clone(&self.said);
        let spoken = Arc::clone(&self.spoken);
        let mut view = calls.view();
        tokio::spawn(async move {
            let mut last_sound = None;
            let mut last_speech = None;
            loop {
                // Copied out first: the line takes the view's lock while it answers.
                let now = view.borrow_and_update().clone();
                if let Some(sound) = now
                    .sound
                    .filter(|sound| last_sound.as_ref() != Some(&sound.id))
                {
                    last_sound = Some(sound.id.clone());
                    said.lock().unwrap().push(sound.said);
                    assert!(calls.answer(&sound.id).is_some(), "the sound is offered");
                    sleep(length).await;
                    calls.played(&sound.id);
                    continue;
                }
                if let Some(speech) = now
                    .speech
                    .filter(|speech| last_speech.as_ref() != Some(&speech.id))
                {
                    last_speech = Some(speech.id.clone());
                    spoken.lock().unwrap().push(speech.text);
                    sleep(length).await;
                    calls.played(&speech.id);
                    continue;
                }
                if view.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// What each sound the phone played said.
    fn said(&self) -> Vec<Said> {
        self.said.lock().unwrap().clone()
    }

    /// The lines the phone said in its own voice.
    fn spoken(&self) -> Vec<String> {
        self.spoken.lock().unwrap().clone()
    }

    /// Starts a call and gives it half a second of background.
    async fn start(&self) -> CallStatus {
        let status = self.calls.start().await;
        self.post(Instant::now(), quiet(500));
        settle().await;
        status
    }

    /// One second of speech and the 1.2 s of silence that ends it, recorded now.
    async fn speak(&self) {
        let mut samples = loud(1000);
        samples.extend(quiet(1200));
        self.post(Instant::now(), samples);
        settle().await;
    }

    fn script(&self, heard: impl IntoIterator<Item = Heard>) {
        self.base.worker_log().script.extend(heard);
    }

    fn say(&self, line: &str, by: Speaker) -> JoinHandle<Option<bool>> {
        let calls = Arc::clone(&self.calls);
        let line = line.to_string();
        tokio::spawn(async move { calls.say(&line, by).await })
    }

    fn view(&self) -> PhoneView {
        self.calls.view().borrow().clone()
    }

    fn set_chief(&self, state: EffectiveAgentState) {
        self.base
            .fake_panes()
            .set(|s| s.view.panes[0].state = state);
    }

    fn transcribed(&self) -> usize {
        self.base.worker_log().transcribed.len()
    }

    fn tts(&self) -> Vec<String> {
        self.base.state().tts.clone()
    }
}

/// A typed turn, then the chief's answer played on the phone for 2 s: the instant it ended,
/// with the call listening again.
async fn after_an_answer(phone: &Phone) -> Instant {
    phone.auto_play(Duration::from_secs(2));
    phone.start().await;
    phone.speak().await;
    assert_eq!(
        phone.say("answer", Speaker::Chief).await.unwrap(),
        Some(true)
    );
    settle().await;
    assert_eq!(phone.calls.status().state, CallState::Listening);
    Instant::now()
}

/// Past the view's next copy of the call's status.
async fn after_a_poll() {
    sleep(STATUS_EVERY).await;
    settle().await;
}

// --- Not hearing the answer ---

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn audio_posted_while_an_answer_plays_is_not_heard() {
    let phone = Phone::new();
    phone.script([text("first"), text("second")]);
    phone.auto_play(Duration::from_secs(2));
    phone.start().await;
    phone.speak().await;
    let said = phone.say("answer", Speaker::Chief);
    settle().await;
    sleep(Duration::from_millis(500)).await;
    assert_eq!(phone.calls.status().state, CallState::Speaking);

    phone.speak().await;
    assert_eq!(said.await.unwrap(), Some(true));
    settle().await;
    past_the_tail().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.transcribed(), 1);
    assert_eq!(phone.base.fake_panes().keys(), typed("%2", "first"));
    assert_eq!(phone.calls.status().state, CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn audio_recorded_during_the_answer_but_arriving_after_it_ends_is_not_heard() {
    let phone = Phone::new();
    phone.script([text("first"), text("second")]);
    let ended = after_an_answer(&phone).await;

    sleep(Duration::from_secs(1)).await;
    // Recorded from 0.5 s before the answer ended to 0.2 s after, all inside the muted time.
    let mut samples = loud(700);
    samples.extend(quiet(1200));
    phone.post(ended - Duration::from_millis(500), samples);
    settle().await;

    assert_eq!(phone.transcribed(), 1);
    assert_eq!(phone.base.fake_panes().keys(), typed("%2", "first"));
    assert_eq!(phone.calls.status().state, CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn audio_recorded_within_0_3_s_of_the_answers_end_is_not_heard() {
    let phone = Phone::new();
    phone.script([text("first"), text("second")]);
    let ended = after_an_answer(&phone).await;

    // A quarter of a second of speech from 50 ms after the end, and silence enough to end
    // it: a turn, were it heard.
    sleep(Duration::from_millis(50)).await;
    let mut samples = loud(250);
    samples.extend(quiet(1500));
    phone.post(ended + Duration::from_millis(50), samples);
    settle().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.transcribed(), 1);
    assert_eq!(phone.base.fake_panes().keys(), typed("%2", "first"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn audio_recorded_0_4_s_after_the_answers_end_is_heard_even_when_it_arrives_late() {
    let phone = Phone::new();
    phone.script([text("first"), text("second")]);
    let ended = after_an_answer(&phone).await;

    sleep(Duration::from_secs(2)).await;
    let mut samples = loud(1000);
    samples.extend(quiet(1200));
    phone.post(ended + Duration::from_millis(400), samples);
    settle().await;

    assert_eq!(phone.transcribed(), 2);
    let mut want = typed("%2", "first");
    want.extend(typed("%2", "second"));
    assert_eq!(phone.base.fake_panes().keys(), want);
}

// --- Harold's phrases on the phone ---

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_busy_chief_gets_the_busy_phrase_on_the_phone() {
    let phone = Phone::new();
    phone.script([text("approve q19")]);
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;
    phone.set_chief(EffectiveAgentState::Busy);

    phone.speak().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.said(), [Said::Phrase(CHIEF_BUSY.into())]);
    assert!(phone.base.fake_panes().keys().is_empty());
    assert!(phone.tts().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_slow_chief_gets_ticks_and_the_threads_phrase_at_90_s() {
    let phone = Phone::new();
    phone.auto_play(Duration::from_millis(500));
    phone.start().await;
    phone.speak().await;
    let typed_at = Instant::now();
    assert_eq!(phone.base.fake_panes().keys().len(), 2);

    sleep_until(typed_at + Duration::from_millis(2_900)).await;
    assert!(phone.said().is_empty());
    sleep_until(typed_at + Duration::from_millis(3_100)).await;
    assert_eq!(phone.said(), [Said::Tick]);
    sleep_until(typed_at + Duration::from_millis(6_100)).await;
    assert_eq!(phone.said(), [Said::Tick, Said::Tick]);

    sleep_until(typed_at + Duration::from_millis(89_900)).await;
    let said = phone.said();
    assert_eq!(said.len(), 29);
    assert!(said.iter().all(|said| *said == Said::Tick));
    sleep_until(typed_at + Duration::from_millis(90_100)).await;
    assert_eq!(
        phone.said().last(),
        Some(&Said::Phrase(NO_ANSWER_IN_THREAD.into()))
    );
    assert!(phone.tts().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_without_harold_speak_plays_the_turn_end_update_on_the_phone() {
    let phone = Phone::new();
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;
    phone.speak().await;

    phone.set_chief(EffectiveAgentState::Busy);
    sleep(Duration::from_millis(1000)).await;
    phone.set_chief(EffectiveAgentState::Idle);
    sleep(Duration::from_secs(4)).await;

    let update = Said::Line {
        text: UPDATE.into(),
        by: Speaker::Chief,
    };
    assert_eq!(phone.said(), [Said::Tick, update]);
    assert_eq!(phone.base.state().update_prompts, ["🎙 hello there"]);
    assert!(phone.tts().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn nothing_heard_plays_sorry_on_the_phone() {
    let phone = Phone::new();
    phone.script([text("")]);
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;

    phone.speak().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.said(), [Said::Phrase(NOTHING_HEARD.into())]);
    assert_eq!(
        phone.calls.status().last_result,
        Some(TurnResult::NothingHeard)
    );
    assert!(phone.tts().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_chief_at_start_plays_the_phrase_on_the_phone_and_starts_nothing() {
    let phone = Phone::new();
    let kept = phrase_file(
        &phone.base.phrases_dir(),
        &phone.base.voice_name(),
        NO_CHIEF,
    );
    std::fs::create_dir_all(phone.base.phrases_dir()).unwrap();
    std::fs::write(&kept, NO_CHIEF).unwrap();
    phone.base.fake_panes().set(|s| s.view.panes.clear());
    phone.auto_play(Duration::from_secs(1));

    let status = phone.calls.start().await;
    settle().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(status.state, CallState::Off);
    assert_eq!(status.last_result, Some(TurnResult::NoChief));
    assert_eq!(phone.said(), [Said::Phrase(NO_CHIEF.into())]);
    assert!(phone.tts().is_empty());
    assert_eq!(phone.calls.source_base(), None);
    assert_eq!(phone.base.worker_log().starts, 0);
    assert_eq!(phone.view().state, CallState::Off);
    assert_eq!(phone.view().ended, Some("no_chief"));
    assert!(!phone.calls.is_following());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_restart_phrase_is_spoken_by_the_phone_not_the_mac() {
    let phone = Phone::new();
    phone.script([Heard::Exit]);
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;

    phone.speak().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.spoken(), [VOICE_STOPPED]);
    assert!(phone.said().is_empty());
    assert!(phone.tts().is_empty());
    assert_eq!(phone.base.worker_log().starts, 2);
    assert_eq!(phone.calls.status().state, CallState::Listening);
}

// --- Hanging up from the phone ---

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_phone_bye_alone_types_nothing_and_ends_the_call() {
    let phone = Phone::new();
    phone.script([text("Bye.")]);
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;

    phone.speak().await;
    sleep(Duration::from_secs(2)).await;

    assert!(phone.base.fake_panes().keys().is_empty());
    assert_eq!(phone.said(), [Said::Phrase(BYE.into())]);
    assert_eq!(phone.view().ended, Some("bye"));
    assert_eq!(phone.calls.status().state, CallState::Off);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn approve_q19_bye_types_the_turn_and_ends_the_call() {
    let phone = Phone::new();
    phone.script([text("approve q19, bye")]);
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;

    phone.speak().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.base.fake_panes().keys(), typed("%2", "approve q19"));
    assert_eq!(phone.said(), [Said::Phrase(BYE.into())]);
    assert_eq!(phone.view().ended, Some("bye"));
    assert_eq!(phone.calls.status().state, CallState::Off);
}

// --- The view, the logs and the lines the phone takes ---

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_view_follows_the_call() {
    let phone = Phone::new();
    phone.script([text("hello there")]);
    phone.auto_play(Duration::from_secs(2));

    phone.start().await;
    assert_eq!(phone.view().state, CallState::Listening);
    assert_eq!(phone.view().transcript, None);
    assert_eq!(phone.view().ended, None);

    phone.speak().await;
    after_a_poll().await;
    assert_eq!(phone.view().state, CallState::Waiting);
    assert_eq!(phone.view().transcript.as_deref(), Some("hello there"));

    let said = phone.say("All green.", Speaker::Chief);
    settle().await;
    after_a_poll().await;
    assert_eq!(phone.view().state, CallState::Speaking);
    assert_eq!(phone.view().transcript.as_deref(), Some("hello there"));

    assert_eq!(said.await.unwrap(), Some(true));
    after_a_poll().await;
    assert_eq!(phone.view().state, CallState::Listening);

    phone.calls.stop().await;
    let ended = PhoneView {
        state: CallState::Off,
        transcript: None,
        sound: None,
        speech: None,
        ended: Some("stopped"),
    };
    assert_eq!(phone.view(), ended);
    after_a_poll().await;
    after_a_poll().await;
    assert_eq!(phone.view(), ended);
    assert!(!phone.calls.is_following());

    // The next call no longer shows how the last one ended.
    phone.start().await;
    assert_eq!(phone.view().state, CallState::Listening);
    assert_eq!(phone.view().ended, None);
    assert!(phone.calls.is_following());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_phone_call_logs_its_start_and_end_with_no_content() {
    let logs = CapturedLogs::start();
    let phone = Phone::new();
    phone.script([text("PHONE-CANARY-7731"), text("Bye.")]);
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;
    // A second start joins the open call: no second follower, and no second line.
    assert_eq!(phone.calls.start().await.state, CallState::Listening);

    phone.speak().await;
    let said = phone.say("PHONE-CANARY-7731 heard", Speaker::Chief);
    assert_eq!(said.await.unwrap(), Some(true));
    settle().await;
    past_the_tail().await;
    phone.speak().await;
    sleep(Duration::from_secs(2)).await;

    assert_eq!(phone.calls.status().state, CallState::Off);
    let text = logs.text();
    assert_eq!(text.matches("app call started").count(), 1, "{text}");
    assert!(text.contains("app call ended reason=\"bye\""), "{text}");
    assert!(!text.contains("PHONE-CANARY-7731"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_phone_takes_only_the_chiefs_lines() {
    let phone = Phone::new();
    phone.auto_play(Duration::from_secs(1));
    phone.start().await;
    phone.speak().await;

    assert_eq!(phone.calls.say("x", Speaker::OtherAgent).await, None);
    let calls = Arc::clone(&phone.calls);
    let blocking =
        tokio::task::spawn_blocking(move || calls.say_blocking("x", Speaker::OtherAgent));
    assert_eq!(blocking.await.unwrap(), None);
    assert_eq!(phone.say("y", Speaker::Chief).await.unwrap(), Some(true));

    let line = Said::Line {
        text: "y".into(),
        by: Speaker::Chief,
    };
    assert_eq!(phone.said(), [line]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn talk_now_takes_back_the_answer_on_the_phone_and_listens() {
    let phone = Phone::new();
    phone.auto_play(Duration::from_secs(5));
    phone.start().await;
    phone.speak().await;
    let said = phone.say("a long answer", Speaker::Chief);
    settle().await;
    let answer = phone.view().sound.expect("the answer is offered");

    let tapped_at = Instant::now();
    phone.calls.talk_now();
    settle().await;

    assert_eq!(phone.calls.status().state, CallState::Listening);
    assert_eq!(said.await.unwrap(), Some(true));
    assert_eq!(Instant::now(), tapped_at);
    assert_eq!(phone.view().sound, None);
    assert_eq!(phone.calls.answer(&answer.id), None);
    after_a_poll().await;
    assert_eq!(phone.view().state, CallState::Listening);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_ends_the_phone_call_and_its_follower() {
    let phone = Phone::new();
    phone.start().await;
    assert!(phone.calls.is_following());

    phone.calls.shutdown().await;

    assert!(!phone.calls.is_following());
    assert_eq!(phone.view().state, CallState::Off);
    assert_eq!(phone.view().ended, Some("shutdown"));
    assert_eq!(phone.calls.status().state, CallState::Off);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_phone_with_no_voice_set_up_starts_nothing() {
    let base = FakePorts::new();
    let calls = PhoneCalls::new(base.clone(), false);

    assert!(!calls.set_up());
    assert_eq!(calls.start().await.state, CallState::NotSetUp);
    assert_eq!(calls.talk_now().state, CallState::NotSetUp);
    assert_eq!(calls.view().borrow().state, CallState::NotSetUp);
    assert_eq!(calls.source_base(), None);
    assert!(!calls.is_following());
    assert_eq!(base.worker_log().starts, 0);
}
