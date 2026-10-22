use super::echo_test::{
    HOUR_MS, MAX_BODY_BYTES, MAX_FRAMES, Outcome, Recent, Setup, background, judge, parse_run,
};

/// A run of 25 quiet frames at `quiet`, then `playback` frames, shaped as the page sends.
fn run(kind: &str, quiet: f64, playback: &[f64]) -> String {
    let mut frames = vec![quiet; 25];
    frames.extend_from_slice(playback);
    serde_json::json!({
        "setup": "speaker",
        "kind": kind,
        "frames": frames,
        "playing_from": 25,
        "playing_to": 25 + playback.len(),
        "echo_cancellation": true,
        "standalone": true,
        "user_agent": "test",
    })
    .to_string()
}

/// `total` frames of which `over` are at `loud` and the rest at `low`.
fn mix(total: usize, over: usize, loud: f64, low: f64) -> Vec<f64> {
    (0..total)
        .map(|i| if i < over { loud } else { low })
        .collect()
}

#[test]
fn the_background_is_the_mean_of_the_first_25_frames_with_a_floor_of_30() {
    let mut frames = vec![100.0; 25];
    frames.extend(vec![9000.0; 100]);
    assert_eq!(background(&frames), 100.0);
    assert_eq!(
        background(&vec![10.0; 40]),
        30.0,
        "a quiet room takes the floor"
    );
    let mut ramp: Vec<f64> = (0..25).map(f64::from).collect();
    ramp.extend(vec![1000.0; 10]);
    assert_eq!(background(&ramp), 30.0, "mean 12 is under the floor");
    let two: Vec<f64> = (0..25).map(|i| if i < 13 { 80.0 } else { 40.0 }).collect();
    assert_eq!(background(&two), (13.0 * 80.0 + 12.0 * 40.0) / 25.0);
}

#[test]
fn a_silent_run_with_5_percent_over_three_times_the_background_passes_and_6_percent_fails() {
    // Background 100, so the bar is 300.
    let pass = parse_run(run("silent", 100.0, &mix(100, 5, 400.0, 120.0)).as_bytes()).unwrap();
    let verdict = judge(&pass, None);
    assert_eq!(verdict.outcome, Outcome::Pass);
    assert_eq!(verdict.share_over_bar, Some(0.05));

    let fail = parse_run(run("silent", 100.0, &mix(100, 6, 400.0, 120.0)).as_bytes()).unwrap();
    let verdict = judge(&fail, None);
    assert_eq!(verdict.outcome, Outcome::Fail);
    assert_eq!(verdict.share_over_bar, Some(0.06));
    assert!(verdict.line.contains("fail"), "{}", verdict.line);
    assert!(verdict.line.contains("6%"), "{}", verdict.line);

    // A frame at exactly the bar is not over it.
    let at_bar = parse_run(run("silent", 100.0, &mix(100, 50, 300.0, 120.0)).as_bytes()).unwrap();
    assert_eq!(judge(&at_bar, None).outcome, Outcome::Pass);
}

#[test]
fn a_silent_verdict_reports_the_median_and_95th_percentile_echo_level() {
    let playback: Vec<f64> = (1..=100).map(f64::from).collect();
    let run = parse_run(run("silent", 100.0, &playback).as_bytes()).unwrap();
    let verdict = judge(&run, None);
    assert_eq!(verdict.p95, Some(95.0));
    assert_eq!(verdict.median, Some(50.5));
}

#[test]
fn a_talk_run_with_10_of_15_frames_over_twice_the_silent_p95_passes_and_9_fails() {
    // The silent run's 95th percentile is 200, so the bar is 400.
    let ten = parse_run(run("talk", 100.0, &mix(15, 10, 500.0, 100.0)).as_bytes()).unwrap();
    let verdict = judge(&ten, Some(200.0));
    assert_eq!(verdict.outcome, Outcome::Pass);
    assert_eq!(verdict.voice_found, Some(true));

    let nine = parse_run(run("talk", 100.0, &mix(15, 9, 500.0, 100.0)).as_bytes()).unwrap();
    let verdict = judge(&nine, Some(200.0));
    assert_eq!(verdict.outcome, Outcome::Fail);
    assert_eq!(verdict.voice_found, Some(false));

    // The 10 may sit anywhere inside a 15-frame window, with the rest of the run quiet.
    let mut playback = vec![100.0; 40];
    playback.extend(mix(15, 10, 500.0, 100.0));
    playback.extend(vec![100.0; 40]);
    let late = parse_run(run("talk", 100.0, &playback).as_bytes()).unwrap();
    assert_eq!(judge(&late, Some(200.0)).outcome, Outcome::Pass);

    // Ten frames spread over more than 15 do not count.
    let spread: Vec<f64> = (0..60)
        .map(|i| if i % 5 == 0 { 500.0 } else { 100.0 })
        .collect();
    let spread = parse_run(run("talk", 100.0, &spread).as_bytes()).unwrap();
    assert_eq!(judge(&spread, Some(200.0)).outcome, Outcome::Fail);
}

#[test]
fn a_talk_run_with_no_silent_run_for_its_setup_in_the_last_hour_says_run_silent_first() {
    let talk = parse_run(run("talk", 100.0, &mix(30, 30, 500.0, 100.0)).as_bytes()).unwrap();
    let verdict = judge(&talk, None);
    assert_eq!(verdict.outcome, Outcome::RunSilentFirst);
    assert_eq!(verdict.voice_found, None);
    assert!(verdict.line.contains("silent"), "{}", verdict.line);

    let recent = Recent::default();
    let now = 10 * HOUR_MS;
    assert_eq!(recent.newest_silent_p95(Setup::Speaker, now), None);
    recent.record(Setup::Speaker, now - HOUR_MS - 1, 111.0);
    assert_eq!(
        recent.newest_silent_p95(Setup::Speaker, now),
        None,
        "an hour and a millisecond old"
    );
    recent.record(Setup::Speaker, now - HOUR_MS + 1, 222.0);
    recent.record(Setup::Ear, now - 1, 333.0);
    assert_eq!(recent.newest_silent_p95(Setup::Speaker, now), Some(222.0));
    recent.record(Setup::Speaker, now - 10, 444.0);
    assert_eq!(
        recent.newest_silent_p95(Setup::Speaker, now),
        Some(444.0),
        "the newest wins"
    );
    assert_eq!(recent.newest_silent_p95(Setup::Headphones, now), None);
}

#[test]
fn an_unknown_setup_or_kind_too_many_frames_or_a_big_body_is_refused() {
    let good = run("silent", 100.0, &mix(10, 0, 0.0, 100.0));
    assert!(parse_run(good.as_bytes()).is_ok());
    let other_setup = good.replace("speaker", "kitchen");
    assert!(parse_run(other_setup.as_bytes()).is_err());
    let other_kind = good.replace("silent", "shout");
    assert!(parse_run(other_kind.as_bytes()).is_err());
    assert!(parse_run(b"not json").is_err());

    let at_limit = serde_json::json!({
        "setup": "ear", "kind": "silent", "frames": vec![1.0; MAX_FRAMES],
        "playing_from": 25, "playing_to": MAX_FRAMES, "echo_cancellation": null,
        "standalone": false, "user_agent": "t",
    })
    .to_string();
    assert!(parse_run(at_limit.as_bytes()).is_ok());
    let too_many = at_limit.replacen("\"frames\":[", "\"frames\":[1.0,", 1);
    assert!(parse_run(too_many.as_bytes()).is_err());

    let padded = format!("{good}{}", " ".repeat(MAX_BODY_BYTES));
    assert!(parse_run(padded.as_bytes()).is_err());

    // The playback window must lie inside the frames and hold at least one.
    let outside = good.replace("\"playing_to\":35", "\"playing_to\":36");
    assert!(parse_run(outside.as_bytes()).is_err());
    let empty = good.replace("\"playing_to\":35", "\"playing_to\":25");
    assert!(parse_run(empty.as_bytes()).is_err());
}
