//! WAV bytes for a turn's samples, and the soft tick played while waiting.

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
}
