//! The conversation core shared by the desk and, later, away-web-app's phone call:
//! it hears the chief speak, finds where each turn ends, and writes it down as audio.

mod hang_up;
mod mac;
mod speech;
mod wav;
mod worker;

use std::future::Future;
use std::path::Path;

/// The sample rate of the audio the call listens to.
pub(crate) const SAMPLE_RATE: u32 = 16_000;

/// Where a call's audio comes from: 16 kHz mono samples as `i16` values (the byte order of
/// whatever carries them is the source's business, not the caller's).
pub(crate) trait AudioSource: Send + 'static {
    /// The next block of samples, of any length, an empty block included. `None` means the
    /// source has ended (ffmpeg exited, the phone hung up), and every later call returns `None`.
    ///
    /// Cancel-safe: dropping the future before it resolves loses no samples and shifts none.
    fn next_block(&mut self) -> impl Future<Output = Option<Vec<i16>>> + Send;
}

/// Where a call's sound goes. The core plays one file at a time and knows when each ends.
pub(crate) trait AudioSink: Send + Sync + 'static {
    /// Plays the 16-bit PCM WAV at `wav` (at any sample rate). One `play` runs at a time.
    /// Resolves `Ok` when the sound has ended or `stop` ended it, and `Err` when it could not
    /// be played.
    fn play(&self, wav: &Path) -> impl Future<Output = Result<(), String>> + Send;
    /// Stops what `play` is playing now, and returns at once; the sound stops promptly. Does
    /// nothing when nothing plays, and does not affect a later `play`.
    fn stop(&self);
}
