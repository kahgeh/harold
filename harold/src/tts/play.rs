//! How a machine plays the audio of a line: Harold writes it to a temporary WAV file (mode
//! 0600), runs `afplay` on the Mac or `pw-play` on Linux, and deletes the file after it plays.

use super::Audio;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The program that plays a WAV file on this machine.
pub(crate) const PLAYER: &str = if cfg!(target_os = "macos") {
    "afplay"
} else {
    "pw-play"
};

/// A 16-bit mono WAV file holding `audio`.
pub(crate) fn wav_bytes(audio: &Audio) -> Vec<u8> {
    let data = u32::try_from(audio.pcm.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + audio.pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&audio.sample_rate.to_le_bytes());
    out.extend_from_slice(&(audio.sample_rate.saturating_mul(2)).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // bytes for each frame
    out.extend_from_slice(&16u16.to_le_bytes()); // bits for each sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(&audio.pcm);
    out
}

/// A WAV file in the temporary folder that is deleted when this is dropped.
pub(crate) struct TempWav {
    path: PathBuf,
}

impl TempWav {
    /// Writes `audio` to a new file in `folder`, readable by the user only.
    pub(crate) fn write(folder: &Path, audio: &Audio) -> std::io::Result<Self> {
        let path = folder.join(format!("harold-tts-{}.wav", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let wav = Self { path };
        file.write_all(&wav_bytes(audio))?;
        Ok(wav)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_wav_header_describes_16_bit_mono_pcm() {
        let audio = Audio {
            sample_rate: 24_000,
            pcm: vec![1, 0, 2, 0],
        };

        let wav = wav_bytes(&audio);

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 40);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 48_000);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
        assert_eq!(&wav[44..], [1, 0, 2, 0]);
    }

    #[test]
    fn the_temporary_file_is_for_the_user_only_and_is_deleted_when_dropped() {
        let audio = Audio {
            sample_rate: 16_000,
            pcm: vec![0; 8],
        };

        let wav = TempWav::write(&std::env::temp_dir(), &audio).unwrap();
        let path = wav.path().to_path_buf();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&path).unwrap(), wav_bytes(&audio));
        drop(wav);
        assert!(!path.exists());
    }

    #[test]
    fn the_player_is_afplay_on_the_mac_and_pw_play_on_linux() {
        assert_eq!(
            PLAYER,
            if cfg!(target_os = "macos") {
                "afplay"
            } else {
                "pw-play"
            }
        );
    }
}
