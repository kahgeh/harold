//! WAV bytes for a turn's samples, and the soft tick, the ringback and the pick-up played while waiting.

use std::f64::consts::PI;

/// A 16-bit mono PCM WAV file's bytes.
pub(crate) fn wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// The soft tick: 40 ms of 880 Hz falling away exponentially, peak 0.2 of full scale, at 24 kHz.
pub(crate) fn tick_wav() -> Vec<u8> {
    const RATE: u32 = 24_000;
    const SAMPLES: usize = 960;
    let peak = 0.2 * f64::from(i16::MAX);
    let samples: Vec<i16> = (0..SAMPLES)
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            (peak * (-t * 100.0).exp() * (2.0 * PI * 880.0 * t).sin()) as i16
        })
        .collect();
    wav_bytes(&samples, RATE)
}

/// One burst of `pitches` summed, `samples` long, at the tick's loudness (peak 0.2 of full
/// scale), fading in and out over 20 ms so it does not click. Two pitches share the peak.
fn burst(pitches: &[f64], samples: usize) -> Vec<i16> {
    const RATE: u32 = 24_000;
    const FADE: usize = 480;
    let peak = 0.2 * f64::from(i16::MAX) / pitches.len() as f64;
    (0..samples)
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            let edge = i.min(samples - 1 - i).min(FADE) as f64 / FADE as f64;
            let sum: f64 = pitches.iter().map(|p| (2.0 * PI * p * t).sin()).sum();
            (peak * edge * sum) as i16
        })
        .collect()
}

/// One cycle of the ringback a phone plays while it rings, in the British and Australian
/// pattern: a 400 Hz and 450 Hz pair for 0.4 s, 0.2 s of quiet, the pair again for 0.4 s,
/// then 2.0 s of quiet; 3.0 s in all, at 24 kHz.
pub(crate) fn ringback_wav() -> Vec<u8> {
    const RATE: u32 = 24_000;
    let mut samples = burst(&[400.0, 450.0], 9_600);
    samples.resize(samples.len() + 4_800, 0);
    samples.extend(burst(&[400.0, 450.0], 9_600));
    samples.resize(3 * RATE as usize, 0);
    wav_bytes(&samples, RATE)
}

/// The pick-up: two quick soft notes rising from 660 Hz to 880 Hz, 0.15 s together, at
/// 24 kHz.
pub(crate) fn pickup_wav() -> Vec<u8> {
    let mut samples = burst(&[660.0], 1_800);
    samples.extend(burst(&[880.0], 1_800));
    wav_bytes(&samples, 24_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    #[test]
    fn wav_bytes_has_a_16_bit_mono_header() {
        let bytes = wav_bytes(&[1, -2, 3], 16_000);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32_at(&bytes, 4), 36 + 6);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u16_at(&bytes, 22), 1);
        assert_eq!(u32_at(&bytes, 24), 16_000);
        assert_eq!(u16_at(&bytes, 34), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32_at(&bytes, 40), 6);
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -2);
    }

    #[test]
    fn tick_is_short_and_soft() {
        let bytes = tick_wav();
        assert_eq!(u32_at(&bytes, 24), 24_000);
        assert_eq!(u32_at(&bytes, 40), 960 * 2);
        let peak = bytes[44..]
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 0);
        assert!(f64::from(peak) <= 0.2 * 32767.0);
    }

    fn samples_of(bytes: &[u8]) -> Vec<i16> {
        bytes[44..]
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    fn loudest(samples: &[i16]) -> u16 {
        samples.iter().map(|s| s.unsigned_abs()).max().unwrap()
    }

    #[test]
    fn ringback_is_one_three_second_cycle_of_two_rings() {
        let bytes = ringback_wav();
        assert_eq!(u32_at(&bytes, 24), 24_000);
        let samples = samples_of(&bytes);
        assert_eq!(samples.len(), 72_000);
        let at = |seconds: f64| (seconds * 24_000.0) as usize;
        assert!(loudest(&samples[..at(0.4)]) > 3_000);
        assert!(loudest(&samples[at(0.6)..at(1.0)]) > 3_000);
        assert_eq!(loudest(&samples[at(0.4)..at(0.6)]), 0);
        assert_eq!(loudest(&samples[at(1.0)..]), 0);
        assert!(f64::from(loudest(&samples)) <= 0.2 * 32767.0);
        // Each ring begins and ends quietly.
        assert_eq!(samples[0], 0);
        assert_eq!(samples[at(0.4) - 1], 0);
        assert_eq!(samples[at(0.6)], 0);
    }

    #[test]
    fn pickup_is_two_rising_soft_notes_in_0_15_s() {
        let bytes = pickup_wav();
        assert_eq!(u32_at(&bytes, 24), 24_000);
        let samples = samples_of(&bytes);
        assert_eq!(samples.len(), 3_600);
        assert!(loudest(&samples) > 3_000);
        assert!(f64::from(loudest(&samples)) <= 0.2 * 32767.0);
        assert_eq!(samples[0], 0);
        assert_eq!(samples[1_799], 0);
        let crossings = |part: &[i16]| part.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!(crossings(&samples[1_800..]) > crossings(&samples[..1_800]));
    }
}
