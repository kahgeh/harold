//! Reads a product's `backlog.md` and the chief's `queue.md`, and writes them
//! back. The import keeps every line it reads: a line it cannot place is
//! refused as `unparsed line <n>`, never dropped. Only column-0 lines starting
//! `# ` or `## ` are headings; `\r\n` is read as `\n`.

use super::schema::{
    Item, MAX_ITEMS, Note, NoteKind, QueueListing, validate_item, validate_note,
    validate_queue_text,
};

/// A note's title is its first line, cut to this many characters.
const TITLE_CHARS: usize = 80;
const HELD: &str = "In flight with another session";
const HELD_HEADING: &str = "## In flight with another session (not ranked)";
const LEFT_OUT: &str = "Looked at and left out";
const LEFT_OUT_HEADING: &str = "## Looked at and left out";
/// An item's fields, in the order they must appear.
const FIELDS: [&str; 7] = [
    "value", "size", "risk", "reason", "paths", "after", "evidence",
];
const EVIDENCE_SEPARATOR: &str = "; ";
/// Written for, and skipped in, an empty queue section.
const NOTHING: &str = "Nothing.";

/// What an import holds, by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counts {
    pub items: u32,
    pub rule: u32,
    pub history: u32,
    pub held: u32,
    pub left_out: u32,
    pub now: u32,
    pub parked: u32,
    pub done: u32,
}

/// A parsed `backlog.md`: the items by rank, and the notes as (kind, title,
/// body) in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BacklogImport {
    pub(super) items: Vec<Item>,
    pub(super) notes: Vec<(NoteKind, String, String)>,
}

impl BacklogImport {
    pub(super) fn counts(&self) -> Counts {
        let of = |kind| count(self.notes.iter().filter(|(k, _, _)| *k == kind).count());
        Counts {
            items: count(self.items.len()),
            rule: of(NoteKind::Rule),
            history: of(NoteKind::History),
            held: of(NoteKind::Held),
            left_out: of(NoteKind::LeftOut),
            ..Counts::default()
        }
    }
}

/// A parsed `queue.md`. `date` is the first `YYYY-MM-DD` in the header, the
/// day the done items are dated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueueImport {
    pub(super) note: String,
    pub(super) now: Option<String>,
    pub(super) parked: Vec<String>,
    pub(super) done: Vec<String>,
    pub(super) date: Option<time::Date>,
}

impl QueueImport {
    pub(super) fn counts(&self) -> Counts {
        Counts {
            now: u32::from(self.now.is_some()),
            parked: count(self.parked.len()),
            done: count(self.done.len()),
            ..Counts::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum ParseError {
    /// The 1-based line of the source that has no place.
    #[error("unparsed line {0}")]
    Unparsed(usize),
    /// The field that fails its limit.
    #[error("invalid {0}")]
    Invalid(String),
    #[error("backlog_full")]
    Full,
}

type Parsed<T> = Result<T, ParseError>;

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A source line and its 1-based number.
type Line<'a> = (usize, &'a str);

/// The lines under one `## ` heading, or before the first one.
struct Section<'a> {
    heading: Option<Line<'a>>,
    lines: Vec<Line<'a>>,
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_indented(line: &str) -> bool {
    line.starts_with([' ', '\t']) && !is_blank(line)
}

/// Splits the source at its `## ` headings. A `# ` title is skipped when it is
/// the first line that is not blank, and refused anywhere else.
fn sections(text: &str) -> Parsed<Vec<Section<'_>>> {
    let mut sections = vec![Section {
        heading: None,
        lines: Vec::new(),
    }];
    let mut started = false;
    for (index, line) in text.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let n = index + 1;
        if line.starts_with("# ") {
            if started {
                return Err(ParseError::Unparsed(n));
            }
            started = true;
            continue;
        }
        started |= !is_blank(line);
        if line.starts_with("## ") {
            sections.push(Section {
                heading: Some((n, line)),
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(section) = sections.last_mut() {
            section.lines.push((n, line));
        }
    }
    Ok(sections)
}

/// The lines without the leading and trailing blank ones, joined verbatim.
fn trimmed(lines: &[Line<'_>]) -> String {
    let first = lines.iter().position(|(_, l)| !is_blank(l));
    let last = lines.iter().rposition(|(_, l)| !is_blank(l));
    match (first, last) {
        (Some(first), Some(last)) => joined(&lines[first..=last]),
        _ => String::new(),
    }
}

fn joined(lines: &[Line<'_>]) -> String {
    lines.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n")
}

fn title_of(first_line: &str) -> String {
    first_line.chars().take(TITLE_CHARS).collect()
}

/// The first `YYYY-MM-DD` in the text that is a real date.
fn first_date(text: &str) -> Option<time::Date> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(9)).find_map(|at| {
        let shape = bytes[at..at + 10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
        if !shape {
            return None;
        }
        let part = |from: usize, to: usize| text[at + from..at + to].parse::<u16>().ok();
        let month = time::Month::try_from(u8::try_from(part(5, 7)?).ok()?).ok()?;
        let day = u8::try_from(part(8, 10)?).ok()?;
        time::Date::from_calendar_date(i32::from(part(0, 4)?), month, day).ok()
    })
}

/// `<n>. <rest>` → `rest`.
fn numbered_text(line: &str) -> Option<&str> {
    let (number, rest) = line.split_once(". ")?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some(rest)
}

pub(super) fn parse_backlog(text: &str) -> Parsed<BacklogImport> {
    let mut items: Vec<Item> = Vec::new();
    let mut notes = Vec::new();
    for section in sections(text)? {
        let Some((n, heading)) = section.heading else {
            notes.extend(rules(&section.lines));
            continue;
        };
        let name = &heading["## ".len()..];
        if let Some((id, title)) = numbered_text(name).and_then(|rest| rest.split_once(": ")) {
            items.push(parse_item(id, title, &section.lines)?);
        } else if name.starts_with(HELD) {
            notes.extend(blocks(NoteKind::Held, &section.lines)?);
        } else if name.starts_with(LEFT_OUT) {
            notes.extend(blocks(NoteKind::LeftOut, &section.lines)?);
        } else if first_date(name).is_some() {
            notes.push((NoteKind::History, name.to_string(), trimmed(&section.lines)));
        } else {
            return Err(ParseError::Unparsed(n));
        }
    }
    for (index, item) in items.iter().enumerate() {
        validate_item(item).map_err(ParseError::Invalid)?;
        // A repeated id would replace the earlier item in the store.
        if items[..index].iter().any(|earlier| earlier.id == item.id) {
            return Err(ParseError::Invalid("id".into()));
        }
    }
    if items.len() > MAX_ITEMS {
        return Err(ParseError::Full);
    }
    for (_, title, body) in &notes {
        validate_note(title, body).map_err(ParseError::Invalid)?;
    }
    Ok(BacklogImport { items, notes })
}

/// Each paragraph before the first heading is a `rule` note.
fn rules(lines: &[Line<'_>]) -> Vec<(NoteKind, String, String)> {
    lines
        .split(|(_, l)| is_blank(l))
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| (NoteKind::Rule, title_of(paragraph[0].1), joined(paragraph)))
        .collect()
}

/// `- <field>: <value>` → the field's index and the value. The value must
/// start on this line, so the export writes the line back as it was.
fn field_line(line: &str) -> Option<(usize, &str)> {
    let (name, value) = line.strip_prefix("- ")?.split_once(": ")?;
    let index = FIELDS.iter().position(|field| *field == name)?;
    (!is_blank(value)).then_some((index, value))
}

fn parse_item(id: &str, title: &str, lines: &[Line<'_>]) -> Parsed<Item> {
    let mut values: [Option<String>; FIELDS.len()] = Default::default();
    let mut last: Option<usize> = None;
    // An indented line continues the field above it, up to a blank line.
    let mut open = false;
    for &(n, line) in lines {
        if is_blank(line) {
            open = false;
            continue;
        }
        if is_indented(line) {
            let Some(value) = last.filter(|_| open).and_then(|i| values[i].as_mut()) else {
                return Err(ParseError::Unparsed(n));
            };
            value.push('\n');
            value.push_str(line);
            continue;
        }
        let Some((index, value)) = field_line(line) else {
            return Err(ParseError::Unparsed(n));
        };
        if last.is_some_and(|last| index <= last) {
            return Err(ParseError::Unparsed(n));
        }
        values[index] = Some(value.to_string());
        last = Some(index);
        open = true;
    }
    let [value, size, risk, reason, paths, after, evidence] = values.map(Option::unwrap_or_default);
    Ok(Item {
        id: id.into(),
        title: title.into(),
        value,
        size,
        risk,
        reason,
        paths,
        after,
        evidence: if evidence.is_empty() {
            Vec::new()
        } else {
            evidence
                .split(EVIDENCE_SEPARATOR)
                .map(String::from)
                .collect()
        },
        rank: 0,
        updated_at_ms: 0,
    })
}

/// One note per block: a block starts at a column-0 `- ` bullet or a column-0
/// line after a blank line; indented lines, and blank lines followed by one,
/// continue it.
fn blocks(kind: NoteKind, lines: &[Line<'_>]) -> Parsed<Vec<(NoteKind, String, String)>> {
    let mut blocks: Vec<Vec<Line<'_>>> = Vec::new();
    let mut blanks: Vec<Line<'_>> = Vec::new();
    let mut after_blank = true;
    for &(n, line) in lines {
        if is_blank(line) {
            blanks.push((n, line));
            after_blank = true;
            continue;
        }
        if is_indented(line) {
            let Some(block) = blocks.last_mut() else {
                return Err(ParseError::Unparsed(n));
            };
            block.append(&mut blanks);
            block.push((n, line));
        } else if line.starts_with("- ") || after_blank {
            blanks.clear();
            blocks.push(vec![(n, line)]);
        } else {
            return Err(ParseError::Unparsed(n));
        }
        after_blank = false;
    }
    Ok(blocks
        .iter()
        .map(|block| {
            let first = block[0].1;
            let title = title_of(first.strip_prefix("- ").unwrap_or(first));
            (kind, title, joined(block))
        })
        .collect())
}

pub(super) fn parse_queue(text: &str) -> Parsed<QueueImport> {
    let mut import = QueueImport {
        note: String::new(),
        now: None,
        parked: Vec::new(),
        done: Vec::new(),
        date: None,
    };
    let mut seen: Vec<&str> = Vec::new();
    for section in sections(text)? {
        let Some((n, heading)) = section.heading else {
            import.note = trimmed(&section.lines);
            continue;
        };
        let name = match &heading["## ".len()..] {
            "Now" => "Now",
            "Parked" => "Parked",
            "Done today" | "Done" => "Done",
            _ => return Err(ParseError::Unparsed(n)),
        };
        if seen.contains(&name) {
            return Err(ParseError::Unparsed(n));
        }
        seen.push(name);
        match name {
            "Now" => {
                let mut entries = entries(&section.lines, |l| l.strip_prefix("- "))?;
                if let Some(&(n, _)) = entries.get(1) {
                    return Err(ParseError::Unparsed(n));
                }
                import.now = entries.pop().map(|(_, text)| text);
            }
            "Parked" => import.parked = texts(entries(&section.lines, numbered_text)?),
            _ => import.done = texts(entries(&section.lines, |l| l.strip_prefix("- "))?),
        }
    }
    import.date = first_date(&import.note);
    let all = std::iter::once(&import.note)
        .chain(&import.now)
        .chain(&import.parked)
        .chain(&import.done);
    for text in all {
        validate_queue_text(text).map_err(ParseError::Invalid)?;
    }
    Ok(import)
}

fn texts(entries: Vec<(usize, String)>) -> Vec<String> {
    entries.into_iter().map(|(_, text)| text).collect()
}

/// The section's entries and the line each starts on: a line `marker` takes,
/// and the indented lines right after it. `Nothing.` is skipped.
fn entries(lines: &[Line<'_>], marker: fn(&str) -> Option<&str>) -> Parsed<Vec<(usize, String)>> {
    let mut entries: Vec<(usize, String)> = Vec::new();
    let mut open = false;
    for &(n, line) in lines {
        if is_blank(line) || line == NOTHING {
            open = false;
            continue;
        }
        if is_indented(line) {
            let Some((_, text)) = entries.last_mut().filter(|_| open) else {
                return Err(ParseError::Unparsed(n));
            };
            text.push('\n');
            text.push_str(line);
            continue;
        }
        let Some(text) = marker(line) else {
            return Err(ParseError::Unparsed(n));
        };
        entries.push((n, text.to_string()));
        open = true;
    }
    Ok(entries)
}

/// The backlog as markdown: the title, rules, items by rank, then the history,
/// held and left-out notes by id, one blank line between blocks.
pub(super) fn export_backlog(product: &str, items: &[Item], notes: &[Note]) -> String {
    let mut notes: Vec<&Note> = notes.iter().collect();
    notes.sort_by_key(|note| note.id);
    let of = |kind: NoteKind| notes.iter().filter(move |note| note.kind == kind);
    let mut items: Vec<&Item> = items.iter().collect();
    items.sort_by_key(|item| item.rank);

    let mut blocks = vec![format!("# Backlog: {product}")];
    blocks.extend(of(NoteKind::Rule).map(|note| note.body.clone()));
    blocks.extend(items.iter().map(|item| item_block(item)));
    for note in of(NoteKind::History) {
        blocks.push(format!("## {}", note.title));
        if !note.body.is_empty() {
            blocks.push(note.body.clone());
        }
    }
    for (kind, heading) in [
        (NoteKind::Held, HELD_HEADING),
        (NoteKind::LeftOut, LEFT_OUT_HEADING),
    ] {
        if of(kind).next().is_some() {
            blocks.push(heading.to_string());
            blocks.extend(of(kind).map(|note| note.body.clone()));
        }
    }
    blocks.join("\n\n") + "\n"
}

fn item_block(item: &Item) -> String {
    let evidence = item.evidence.join(EVIDENCE_SEPARATOR);
    let values = [
        &item.value,
        &item.size,
        &item.risk,
        &item.reason,
        &item.paths,
        &item.after,
        &evidence,
    ];
    let mut lines = vec![format!("## {}. {}: {}", item.rank, item.id, item.title)];
    lines.extend(
        FIELDS
            .iter()
            .zip(values)
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| format!("- {name}: {value}")),
    );
    lines.join("\n")
}

/// The queue as markdown: the note, Now, the parked numbered 1..n, and every
/// done item the listing holds; `Nothing.` for an empty section.
pub(super) fn export_queue(listing: &QueueListing) -> String {
    let section = |heading: &str, lines: Vec<String>| {
        let body = if lines.is_empty() {
            NOTHING.to_string()
        } else {
            lines.join("\n")
        };
        format!("## {heading}\n\n{body}")
    };
    let mut blocks = vec!["# Queue".to_string()];
    if !listing.note.is_empty() {
        blocks.push(listing.note.clone());
    }
    let now = listing.now.iter().map(|item| format!("- {}", item.text));
    blocks.push(section("Now", now.collect()));
    let parked = listing
        .parked
        .iter()
        .enumerate()
        .map(|(n, item)| format!("{}. {}", n + 1, item.text));
    blocks.push(section("Parked", parked.collect()));
    let done = listing.done.iter().map(|item| format!("- {}", item.text));
    blocks.push(section("Done", done.collect()));
    blocks.join("\n\n") + "\n"
}

/// The lines the lossless check compares: not blank, not starting with `#`,
/// with `\r` dropped.
#[cfg(test)]
pub(super) fn content_lines(markdown: &str) -> Vec<String> {
    markdown
        .lines()
        .map(|line| line.replace('\r', ""))
        .filter(|line| !is_blank(line) && !line.starts_with('#'))
        .collect()
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
