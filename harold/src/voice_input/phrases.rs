//! Harold's own phrases in a call, and where each is kept as audio between calls.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(crate) const NOTHING_HEARD: &str = "Sorry, I didn't catch that.";
pub(crate) const CHIEF_BUSY: &str = "The chief is busy. I'll pass it on when it's free.";
pub(crate) const NO_ANSWER: &str = "No answer yet.";
/// The phone's version, which knows the answer will be in the thread.
pub(crate) const NO_ANSWER_IN_THREAD: &str = "No answer yet. It will be in the thread.";
/// Said before a line the chief speaks between turns of a call.
pub(crate) const FROM_THE_CHIEF: &str = "From the chief.";
/// Said when the answerer handed a request over and gave no line of its own.
pub(crate) const PASSED_ON: &str = "I've passed that to the chief.";
pub(crate) const BYE: &str = "Bye.";
/// Said through the `[tts]` command, never by the worker.
pub(crate) const VOICE_STOPPED: &str = "The voice stopped. One moment.";
/// Said in the phone's own voice when a call ends because a speech model did not download.
pub(crate) const MODEL_MISSING: &str = "The speech model isn't on the Mac yet.";
pub(crate) use super::shared::{NO_CHIEF, VOICE_FAILED};

/// The file `phrase` is kept in for `voice`: `<first 16 hex of sha256(voice + "\n" + phrase)>.wav`.
pub(crate) fn phrase_file(dir: &Path, voice: &str, phrase: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{voice}\n{phrase}").as_bytes());
    dir.join(format!("{}.wav", &hex::encode(digest)[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phrase_file_is_named_by_its_voice_and_text() {
        let digest = hex::encode(Sha256::digest(b"af_heart\nBye."));
        let file = phrase_file(Path::new("/p"), "af_heart", BYE);
        assert_eq!(file, Path::new("/p").join(format!("{}.wav", &digest[..16])));
        assert_ne!(file, phrase_file(Path::new("/p"), "bf_emma", BYE));
        assert_ne!(file, phrase_file(Path::new("/p"), "af_heart", NO_ANSWER));
    }
}
