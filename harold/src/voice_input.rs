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

/// Where a call's audio comes from: 16 kHz mono 16-bit samples, in blocks of any size.
pub(crate) trait AudioSource: Send + 'static {
    /// The next block, or `None` once the source has ended (ffmpeg exited, the phone hung up).
    fn next_block(&mut self) -> impl Future<Output = Option<Vec<i16>>> + Send;
}

/// Where a call's sound goes. The core plays one file at a time and knows when each ends.
pub(crate) trait AudioSink: Send + Sync + 'static {
    /// Plays the WAV at `wav`; resolves when it has ended, or at once when `stop` is called.
    fn play(&self, wav: &Path) -> impl Future<Output = Result<(), String>> + Send;
    /// Stops what `play` is playing now; does nothing when nothing plays.
    fn stop(&self);
}
