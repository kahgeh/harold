//! Hears a spoken hang-up at the end of a transcript.

/// What a transcript says once the hang-up words are taken off its end.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Spoken<'a> {
    Turn(&'a str),
    TurnThenBye(&'a str),
    ByeOnly,
}

/// The hang-up words, longest first.
const BYE_WORDS: [&[&str]; 4] = [&["bye", "chief"], &["bye", "bye"], &["goodbye"], &["bye"]];

/// Splits a transcript at the hang-up words when they are its last words.
pub(crate) fn hang_up(transcript: &str) -> Spoken<'_> {
    let words = words(transcript);
    for bye in BYE_WORDS {
        let Some(start) = words.len().checked_sub(bye.len()) else {
            continue;
        };
        let tail = &words[start..];
        if !tail
            .iter()
            .zip(bye)
            .all(|((_, word), want)| word.eq_ignore_ascii_case(want))
        {
            continue;
        }
        let before = transcript[..tail[0].0]
            .trim_end_matches(|c: char| c.is_whitespace() || ",.;:!?-".contains(c));
        return if before.is_empty() {
            Spoken::ByeOnly
        } else {
            Spoken::TurnThenBye(before)
        };
    }
    Spoken::Turn(transcript)
}

/// Each word with where it starts: a run of alphanumerics and apostrophes.
fn words(text: &str) -> Vec<(usize, &str)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '\'';
    let mut words = Vec::new();
    let mut start = None;
    for (at, c) in text.char_indices() {
        match (is_word(c), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                words.push((from, &text[from..at]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        words.push((from, &text[from..]));
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bye_alone_hangs_up() {
        assert_eq!(hang_up("Bye."), Spoken::ByeOnly);
        assert_eq!(hang_up("Bye bye!"), Spoken::ByeOnly);
        assert_eq!(hang_up("goodbye"), Spoken::ByeOnly);
        assert_eq!(hang_up("Bye chief."), Spoken::ByeOnly);
    }

    #[test]
    fn a_turn_then_bye_keeps_the_turn() {
        assert_eq!(
            hang_up("approve q19, bye"),
            Spoken::TurnThenBye("approve q19")
        );
        assert_eq!(hang_up("OK, bye chief."), Spoken::TurnThenBye("OK"));
        assert_eq!(hang_up("yes - bye bye!"), Spoken::TurnThenBye("yes"));
        assert_eq!(hang_up("fine. Goodbye"), Spoken::TurnThenBye("fine"));
    }

    #[test]
    fn bye_inside_a_turn_is_not_a_hang_up() {
        let text = "say bye to the old store first";
        assert_eq!(hang_up(text), Spoken::Turn(text));
    }

    #[test]
    fn an_empty_transcript_is_an_empty_turn() {
        assert_eq!(hang_up(""), Spoken::Turn(""));
    }
}
