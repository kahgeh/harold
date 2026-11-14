//! The checks of the fields of a decision record. Every check gives a yes or no, and the
//! service picks the reason.

use time::{Date, Month, OffsetDateTime, format_description::well_known::Rfc3339};

/// The most characters of a title.
pub(super) const TITLE_MAX: usize = 120;

/// `YYYY-MM-DD` that is a real date.
pub(super) fn date(text: &str) -> Option<Date> {
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let number = |part: &str| {
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let month = Month::try_from(u8::try_from(number(month)?).ok()?).ok()?;
    Date::from_calendar_date(
        i32::try_from(number(year)?).ok()?,
        month,
        u8::try_from(number(day)?).ok()?,
    )
    .ok()
}

/// An RFC 3339 time, or the short form `YYYY-MM-DDTHH:MMZ`, in milliseconds since the
/// epoch.
pub(super) fn time_ms(text: &str) -> Option<i64> {
    let full;
    let text = if text.len() == 17 && text.ends_with('Z') {
        full = format!("{}:00Z", &text[..16]);
        full.as_str()
    } else {
        text
    };
    let moment = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    i64::try_from(moment.unix_timestamp_nanos() / 1_000_000).ok()
}

/// The date in a well-formed id `d-YYYYMMDD-NN` (two digits or more for NN).
pub(super) fn id_date(id: &str) -> Option<Date> {
    let rest = id.strip_prefix("d-")?;
    let (day, number) = rest.split_once('-')?;
    if day.len() != 8
        || number.len() < 2
        || !day
            .bytes()
            .chain(number.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    date(&format!("{}-{}-{}", &day[..4], &day[4..6], &day[6..]))
}

/// The id after the last `#` of a details value.
pub(super) fn anchor_id(details: &str) -> Option<&str> {
    details
        .rsplit_once('#')
        .map(|(_, id)| id)
        .filter(|id| !id.is_empty())
}

fn word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `chief`, or `pnem:<product>`.
pub(super) fn details_owner(text: &str) -> bool {
    text == "chief" || text.strip_prefix("pnem:").is_some_and(word)
}

/// A title is one line of 1 to `TITLE_MAX` characters.
pub(super) fn title(text: &str) -> bool {
    let count = text.chars().count();
    !text.trim().is_empty() && count <= TITLE_MAX && !text.contains(['\n', '\r'])
}

/// Empty, `work:<name>`, or `spec:<product>/<id>`.
pub(super) fn link(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    if let Some(name) = text.strip_prefix("work:") {
        return word(name);
    }
    text.strip_prefix("spec:")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(product, id)| word(product) && word(id))
}
