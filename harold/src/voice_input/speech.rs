//! Finds where each spoken turn starts and ends, from how loud each 20 ms frame is.

/// Samples in one 20 ms frame at 16 kHz.
pub(crate) const FRAME: usize = 320;
/// Frames, not muted, that set the background level.
pub(crate) const CALIBRATION_FRAMES: usize = 25;
/// Loud frames in a row that start speech.
pub(crate) const START_FRAMES: usize = 10;
/// Quiet frames in a row that end speech.
pub(crate) const END_FRAMES: usize = 60;
/// Frames after which a turn is cut.
pub(crate) const MAX_TURN_FRAMES: usize = 3000;
/// How many times the background a frame must be to count as loud.
pub(crate) const LOUDNESS_FACTOR: f64 = 3.0;
/// The lowest background level taken, so a silent room does not give a threshold of 0.
pub(crate) const BACKGROUND_FLOOR: f64 = 30.0;

/// Splits a stream of 16 kHz samples into the turns spoken in it.
pub(crate) struct SpeechDetector {
    background: Option<f64>,
    calibration_sum: f64,
    calibration_count: usize,
    partial: Vec<i16>,
    loud_run: usize,
    pending: Vec<i16>,
    in_turn: bool,
    turn: Vec<i16>,
    tail: Vec<i16>,
    turn_frames: usize,
    quiet_run: usize,
}

impl SpeechDetector {
    pub(crate) fn new() -> Self {
        Self {
            background: None,
            calibration_sum: 0.0,
            calibration_count: 0,
            partial: Vec::new(),
            loud_run: 0,
            pending: Vec::new(),
            in_turn: false,
            turn: Vec::new(),
            tail: Vec::new(),
            turn_frames: 0,
            quiet_run: 0,
        }
    }

    /// Feeds a block of any length. `muted` marks every frame in it as silence that is not kept.
    /// Returns each turn the block ended or cut, as its samples.
    pub(crate) fn push(&mut self, block: &[i16], muted: bool) -> Vec<Vec<i16>> {
        let mut turns = Vec::new();
        let mut rest = block;
        while !rest.is_empty() {
            let take = (FRAME - self.partial.len()).min(rest.len());
            self.partial.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.partial.len() < FRAME {
                break;
            }
            let frame = std::mem::take(&mut self.partial);
            turns.extend(self.frame(&frame, muted));
        }
        turns
    }

    /// Forgets any speech in progress, keeping the background level.
    pub(crate) fn reset(&mut self) {
        self.partial.clear();
        self.clear_speech();
    }

    /// Whether speech has started and the turn has not yet ended.
    pub(crate) fn in_turn(&self) -> bool {
        self.in_turn
    }

    fn clear_speech(&mut self) {
        self.loud_run = 0;
        self.pending.clear();
        self.in_turn = false;
        self.turn.clear();
        self.tail.clear();
        self.turn_frames = 0;
        self.quiet_run = 0;
    }

    fn frame(&mut self, frame: &[i16], muted: bool) -> Option<Vec<i16>> {
        let Some(background) = self.background else {
            if !muted {
                self.calibration_sum += rms(frame);
                self.calibration_count += 1;
                if self.calibration_count == CALIBRATION_FRAMES {
                    let mean = self.calibration_sum / CALIBRATION_FRAMES as f64;
                    self.background = Some(mean.max(BACKGROUND_FLOOR));
                }
            }
            return None;
        };
        let loud = !muted && rms(frame) > LOUDNESS_FACTOR * background;
        if !self.in_turn {
            if !loud {
                self.loud_run = 0;
                self.pending.clear();
                return None;
            }
            self.pending.extend_from_slice(frame);
            self.loud_run += 1;
            if self.loud_run >= START_FRAMES {
                self.in_turn = true;
                self.turn = std::mem::take(&mut self.pending);
                self.turn_frames = self.loud_run;
                self.quiet_run = 0;
            }
            return None;
        }
        self.turn_frames += 1;
        if loud {
            self.turn.append(&mut self.tail);
            self.turn.extend_from_slice(frame);
            self.quiet_run = 0;
        } else {
            self.quiet_run += 1;
            if !muted {
                self.tail.extend_from_slice(frame);
            }
        }
        if self.quiet_run >= END_FRAMES || self.turn_frames >= MAX_TURN_FRAMES {
            let turn = std::mem::take(&mut self.turn);
            self.clear_speech();
            return Some(turn);
        }
        None
    }
}

fn rms(frame: &[i16]) -> f64 {
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / frame.len() as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loud(ms: usize) -> Vec<i16> {
        (0..ms * 16)
            .map(|i| (8000.0 * (i as f64 * 0.3).sin()) as i16)
            .collect()
    }

    fn quiet(ms: usize) -> Vec<i16> {
        vec![0; ms * 16]
    }

    fn feed(detector: &mut SpeechDetector, parts: &[Vec<i16>], block: usize) -> Vec<Vec<i16>> {
        let all: Vec<i16> = parts.concat();
        all.chunks(block)
            .flat_map(|chunk| detector.push(chunk, false))
            .collect()
    }

    fn first_case() -> Vec<Vec<i16>> {
        vec![quiet(500), loud(1000), quiet(1200)]
    }

    #[test]
    fn a_frame_is_20_ms_at_the_sample_rate() {
        assert_eq!(FRAME as u32, crate::voice_input::SAMPLE_RATE / 50);
    }

    #[test]
    fn speech_then_1_2_s_of_silence_ends_one_turn() {
        let mut detector = SpeechDetector::new();
        let turns = feed(&mut detector, &first_case(), 100_000);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].len(), 50 * FRAME);
        assert!(!detector.in_turn());
    }

    #[test]
    fn one_second_of_silence_inside_speech_keeps_one_turn() {
        let mut detector = SpeechDetector::new();
        let parts = vec![quiet(500), loud(1000), quiet(1000), loud(1000), quiet(1200)];
        let turns = feed(&mut detector, &parts, 100_000);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].len(), 150 * FRAME);
    }

    #[test]
    fn a_turn_is_cut_at_60_seconds() {
        let mut detector = SpeechDetector::new();
        let mut turns = detector.push(&quiet(500), false);
        turns.extend(detector.push(&loud(70_000), false));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].len(), MAX_TURN_FRAMES * FRAME);

        // Cut: a fresh 0.2 s of loud is needed before the next turn starts.
        let mut detector = SpeechDetector::new();
        detector.push(&quiet(500), false);
        let turns = detector.push(&loud(60_000), false);
        assert_eq!(turns.len(), 1);
        assert!(!detector.in_turn());
        detector.push(&loud(180), false);
        assert!(!detector.in_turn());
        detector.push(&loud(20), false);
        assert!(detector.in_turn());
    }

    #[test]
    fn less_than_0_2_s_of_noise_starts_no_turn() {
        let mut detector = SpeechDetector::new();
        let parts = vec![quiet(500), loud(180), quiet(1500)];
        assert!(feed(&mut detector, &parts, 100_000).is_empty());
        assert!(!detector.in_turn());
    }

    #[test]
    fn muted_frames_start_no_turn() {
        let mut detector = SpeechDetector::new();
        detector.push(&quiet(500), false);
        assert!(detector.push(&loud(1000), true).is_empty());
        assert!(!detector.in_turn());
    }

    #[test]
    fn blocks_of_any_size_give_the_same_turns() {
        let mut small = SpeechDetector::new();
        let mut whole = SpeechDetector::new();
        let a = feed(&mut small, &first_case(), 7);
        let b = feed(&mut whole, &first_case(), 1_000_000);
        assert_eq!(a.len(), 1);
        assert_eq!(a, b);
    }

    #[test]
    fn reset_forgets_speech_but_keeps_the_background() {
        let mut detector = SpeechDetector::new();
        detector.push(&quiet(500), false);
        detector.push(&loud(400), false);
        assert!(detector.in_turn());
        detector.reset();
        assert!(!detector.in_turn());
        detector.push(&loud(200), false);
        assert!(detector.in_turn());
    }
}
