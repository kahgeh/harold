//! The conversation core shared by the desk and, later, away-web-app's phone call:
//! it hears the chief speak, finds where each turn ends, and writes it down as audio.

mod hang_up;
mod speech;
mod wav;

/// The sample rate of the audio the call listens to.
pub(crate) const SAMPLE_RATE: u32 = 16_000;
