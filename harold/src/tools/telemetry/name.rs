//! Names, kinds, value lists and the refusals. Pure: no store, no clock.

use serde::{Deserialize, Serialize};

/// The longest name.
pub(crate) const MAX_NAME: usize = 64;
/// The most series for a counter or a gauge.
pub(crate) const MAX_SERIES: u64 = 100;
/// The most attribute keys for a name.
pub(crate) const MAX_KEYS: usize = 3;
/// The most characters in a meaning.
pub(crate) const MAX_MEANING: usize = 80;
/// The most names for one prefix.
pub(crate) const MAX_NAMES: u32 = 50;
/// The most events among them.
pub(crate) const MAX_EVENTS: u32 = 20;

const MAX_KEY: usize = 32;
const MAX_VALUE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    Counter,
    Gauge,
    Event,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Event => "event",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "counter" => Some(Kind::Counter),
            "gauge" => Some(Kind::Gauge),
            "event" => Some(Kind::Event),
            _ => None,
        }
    }
}

/// Why a call is refused. The first eleven cases are the spec's reason words;
/// `Invalid` is malformed input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    NoPane,
    WrongPrefix {
        prefix: String,
    },
    UnknownPrefix {
        prefix: String,
    },
    TooManyNames {
        prefix: String,
    },
    TooManyEvents {
        prefix: String,
    },
    TooManySeries {
        name: String,
        series: u64,
    },
    UndeclaredValue {
        key: String,
        value: String,
        allowed: String,
    },
    WrongKind {
        name: String,
        declared: Kind,
        asked: Kind,
    },
    Undeclared {
        name: String,
    },
    RateLimited {
        prefix: String,
    },
    MeaningTooLong {
        length: usize,
    },
    DeclarationDiffers {
        name: String,
    },
    Invalid(&'static str),
}

impl Refused {
    /// The fixed reason word.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Refused::NoPane => "no_pane",
            Refused::WrongPrefix { .. } => "wrong_prefix",
            Refused::UnknownPrefix { .. } => "unknown_prefix",
            Refused::TooManyNames { .. } | Refused::TooManyEvents { .. } => "too_many_names",
            Refused::TooManySeries { .. } => "too_many_series",
            Refused::UndeclaredValue { .. } => "undeclared_value",
            Refused::WrongKind { .. } => "wrong_kind",
            Refused::Undeclared { .. } => "undeclared",
            Refused::RateLimited { .. } => "rate_limited",
            Refused::MeaningTooLong { .. } => "meaning_too_long",
            Refused::DeclarationDiffers { .. } => "declaration_differs",
            Refused::Invalid(_) => "invalid",
        }
    }

    /// The plain text, as in the spec's "Refusals in plain words".
    pub(crate) fn text(&self) -> String {
        match self {
            Refused::NoPane => "run this from a pane: no TMUX_PANE or REX_BLOCK".into(),
            Refused::WrongPrefix { prefix } if prefix == "chief" => {
                "chief. is for the chief's pane".into()
            }
            Refused::WrongPrefix { prefix } => {
                format!("{prefix}. is for panes in {prefix}'s folder and the chief")
            }
            Refused::UnknownPrefix { prefix } => {
                format!("unknown prefix {prefix}.: use chief. or a product name")
            }
            Refused::TooManyNames { prefix } => {
                format!("{prefix}. has {MAX_NAMES} names: remove one first")
            }
            Refused::TooManyEvents { prefix } => {
                format!("{prefix}. has {MAX_EVENTS} events: remove one first")
            }
            Refused::TooManySeries { name, series } => {
                format!("{name} would have {series} series: the most is {MAX_SERIES}")
            }
            Refused::UndeclaredValue {
                key,
                value,
                allowed,
            } => {
                format!("{key}={value} is not declared: use {allowed}")
            }
            Refused::WrongKind {
                name,
                declared,
                asked,
            } => {
                format!(
                    "{name} is a {}, not a {}",
                    declared.as_str(),
                    asked.as_str()
                )
            }
            Refused::Undeclared { name } => {
                format!("{name} is not declared: declare it first")
            }
            Refused::RateLimited { prefix } => {
                format!("{prefix}. wrote 60 times this minute: wait")
            }
            Refused::MeaningTooLong { length } => {
                format!("the meaning has {length} characters: the most is {MAX_MEANING}")
            }
            Refused::DeclarationDiffers { name } => {
                format!("{name} is declared with other values: remove it and declare it again")
            }
            Refused::Invalid(text) => (*text).into(),
        }
    }
}

/// A valid name and its prefix (the part before the first dot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Name {
    pub full: String,
    pub prefix: String,
}

const BAD_NAME: &str = "invalid name: use a prefix, a dot and a rest, from a-z, 0-9, . and _, \
                        at most 64 characters";

/// Checks the name: lower case `a-z`, `0-9`, `.`, `_`, at most 64 characters,
/// and at least two parts with no part empty.
pub(crate) fn parse_name(raw: &str) -> Result<Name, Refused> {
    let characters_ok = raw
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
    let parts_ok = raw.split('.').count() >= 2 && raw.split('.').all(|part| !part.is_empty());
    if raw.len() > MAX_NAME || !characters_ok || !parts_ok {
        return Err(Refused::Invalid(BAD_NAME));
    }
    let prefix = raw.split('.').next().unwrap_or_default().to_string();
    Ok(Name {
        full: raw.to_string(),
        prefix,
    })
}

/// The allowed values of one attribute key. A range is never expanded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ValueList {
    Values(Vec<String>),
    Range { lo: u32, hi: u32 },
}

impl ValueList {
    /// How many values the list holds.
    pub(crate) fn count(&self) -> u64 {
        match self {
            ValueList::Values(values) => values.len() as u64,
            ValueList::Range { lo, hi } => u64::from(*hi) - u64::from(*lo) + 1,
        }
    }

    /// Whether the value is in the list. A range takes whole numbers in their
    /// plain form only, so `007` is not in `0-50`.
    pub(crate) fn contains(&self, value: &str) -> bool {
        match self {
            ValueList::Values(values) => values.iter().any(|v| v == value),
            ValueList::Range { lo, hi } => value
                .parse::<u32>()
                .is_ok_and(|n| n.to_string() == value && (*lo..=*hi).contains(&n)),
        }
    }

    /// The list as a hint, such as `ok or failed` or `0-50`.
    pub(crate) fn describe(&self) -> String {
        match self {
            ValueList::Range { lo, hi } => format!("{lo}-{hi}"),
            ValueList::Values(values) => match values.split_last() {
                Some((last, [])) => last.clone(),
                Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                None => String::new(),
            },
        }
    }
}

/// One attribute key and its allowed values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attr {
    pub key: String,
    pub values: ValueList,
}

const BAD_VALUES: &str =
    "invalid values: use 1 or more of a-z, 0-9, _, . and -, or a range such as 0-50";
const BAD_RANGE: &str = "invalid range: use two whole numbers, low first, such as 0-50";
const BAD_KEY: &str = "invalid attribute key: use a-z, 0-9 and _, at most 32 characters";
const BAD_KEYS: &str = "invalid attributes: at most 3 keys, each key once";

/// A whole number from digits; too many digits give the largest number.
fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.parse::<u32>().unwrap_or(u32::MAX))
}

/// Reads the values of one key. One value that starts with a digit and holds a
/// `-` is a range `lo-hi`; anything else is a list of plain values.
pub(crate) fn parse_values(raw: &[String]) -> Result<ValueList, Refused> {
    if let [single] = raw
        && single.starts_with(|c: char| c.is_ascii_digit())
        && let Some((lo, hi)) = single.split_once('-')
    {
        let (Some(lo), Some(hi)) = (digits(lo), digits(hi)) else {
            return Err(Refused::Invalid(BAD_RANGE));
        };
        if lo > hi {
            return Err(Refused::Invalid(BAD_RANGE));
        }
        return Ok(ValueList::Range { lo, hi });
    }
    let plain = |value: &String| {
        !value.is_empty()
            && !value.starts_with('-')
            && value.len() <= MAX_VALUE
            && value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
    };
    let distinct = raw
        .iter()
        .enumerate()
        .all(|(i, value)| !raw[..i].contains(value));
    if raw.is_empty() || !raw.iter().all(plain) || !distinct {
        return Err(Refused::Invalid(BAD_VALUES));
    }
    Ok(ValueList::Values(raw.to_vec()))
}

/// Builds an attribute from a key and its raw values.
pub(crate) fn parse_attr(key: &str, raw: &[String]) -> Result<Attr, Refused> {
    let key_ok = !key.is_empty()
        && key.len() <= MAX_KEY
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !key_ok {
        return Err(Refused::Invalid(BAD_KEY));
    }
    Ok(Attr {
        key: key.to_string(),
        values: parse_values(raw)?,
    })
}

/// At most `MAX_KEYS` keys, and no key twice.
pub(crate) fn check_keys(attrs: &[Attr]) -> Result<(), Refused> {
    let distinct = attrs
        .iter()
        .enumerate()
        .all(|(i, a)| !attrs[..i].iter().any(|b| b.key == a.key));
    if attrs.len() > MAX_KEYS || !distinct {
        return Err(Refused::Invalid(BAD_KEYS));
    }
    Ok(())
}

/// The product of the value counts, saturating. No attribute is one series.
pub(crate) fn series_count(attrs: &[Attr]) -> u64 {
    attrs.iter().fold(1u64, |total, attr| {
        total.saturating_mul(attr.values.count())
    })
}

#[cfg(test)]
#[path = "name_tests.rs"]
mod tests;
