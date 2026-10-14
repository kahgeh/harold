//! The length rule for a line the chief asks Harold to speak.

/// The longest spoken line, in Unicode characters of the one-line text.
pub(crate) const MAX_SPOKEN_CHARS: usize = 300;

/// Whether a one-line text has 1 to 300 characters.
pub(crate) fn is_speakable_length(line: &str) -> bool {
    (1..=MAX_SPOKEN_CHARS).contains(&line.chars().count())
}

#[cfg(test)]
mod tests {
    use super::{MAX_SPOKEN_CHARS, is_speakable_length};

    #[test]
    fn a_line_has_1_to_300_characters() {
        assert!(!is_speakable_length(""));
        assert!(is_speakable_length("x"));
        assert!(is_speakable_length(&"x".repeat(MAX_SPOKEN_CHARS)));
        assert!(!is_speakable_length(&"x".repeat(MAX_SPOKEN_CHARS + 1)));
        assert!(is_speakable_length(&"🤖".repeat(MAX_SPOKEN_CHARS)));
    }
}
