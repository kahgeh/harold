use super::*;
use crate::tools::backlog::schema::{QueueItem, QueueState};

const BACKLOG: &str = include_str!("fixtures/backlog.md");
const QUEUE: &str = include_str!("fixtures/queue.md");

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The parsed items as the store would list them, ranked 1..n.
fn ranked(import: &BacklogImport) -> Vec<Item> {
    import
        .items
        .iter()
        .enumerate()
        .map(|(n, item)| Item {
            rank: n as u32 + 1,
            ..item.clone()
        })
        .collect()
}

/// The parsed notes as the store would list them, with ids 1..n.
fn numbered(import: &BacklogImport) -> Vec<Note> {
    import
        .notes
        .iter()
        .enumerate()
        .map(|(n, (kind, title, body))| Note {
            id: n as i64 + 1,
            kind: *kind,
            title: title.clone(),
            body: body.clone(),
            created_at_ms: 0,
            updated_at_ms: 0,
        })
        .collect()
}

fn queue_item(n: usize, state: QueueState, position: u32, text: &str) -> QueueItem {
    QueueItem {
        id: format!("N{n}"),
        state,
        position,
        text: text.into(),
        done_at_ms: 0,
        updated_at_ms: 0,
    }
}

/// The parsed queue as the store would list it.
fn listed(import: &QueueImport) -> QueueListing {
    QueueListing {
        now: import
            .now
            .as_deref()
            .map(|text| queue_item(1, QueueState::Now, 0, text)),
        parked: import
            .parked
            .iter()
            .enumerate()
            .map(|(n, text)| queue_item(n + 2, QueueState::Parked, n as u32 + 1, text))
            .collect(),
        done: import
            .done
            .iter()
            .enumerate()
            .map(|(n, text)| queue_item(n + 10, QueueState::Done, 0, text))
            .collect(),
        note: import.note.clone(),
    }
}

/// The text with `line` inserted after the first line that starts with
/// `after`, and the inserted line's 1-based number.
fn insert_after(text: &str, after: &str, line: &str) -> (String, usize) {
    let mut lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|l| l.starts_with(after)).unwrap() + 1;
    lines.insert(at, line);
    (lines.join("\n"), at + 1)
}

#[test]
fn the_backlog_fixture_gives_its_counts() {
    let import = parse_backlog(BACKLOG).unwrap();
    assert_eq!(
        import.counts(),
        Counts {
            items: 5,
            rule: 2,
            history: 4,
            held: 3,
            left_out: 3,
            ..Counts::default()
        }
    );
}

#[test]
fn every_item_keeps_every_field_and_pointer() {
    let import = parse_backlog(BACKLOG).unwrap();
    let item = |id: &str, title: &str, fields: [&str; 6], evidence: &[&str]| Item {
        id: id.into(),
        title: title.into(),
        value: fields[0].into(),
        size: fields[1].into(),
        risk: fields[2].into(),
        reason: fields[3].into(),
        paths: fields[4].into(),
        after: fields[5].into(),
        evidence: strings(evidence),
        rank: 0,
        updated_at_ms: 0,
    };
    let expected = vec![
        item(
            "search-filter",
            "Let the list be filtered by a typed word",
            [
                "a long list is hard to scan; a filter finds an entry in seconds",
                "small",
                "the filter could hide an entry the user expects to see",
                "three people asked for it this week",
                "app/src/list.rs, app/src/filter.rs",
                "nothing before it",
            ],
            &["notes/2026-02-28.md:12"],
        ),
        item(
            "export-csv",
            "Write the list out as a CSV file",
            [
                "people can take their entries into a spreadsheet",
                "medium",
                "commas inside an entry must be quoted",
                "the spreadsheet is where the monthly summary is made;\n  \
                 without an export it is typed in by hand",
                "app/src/export.rs",
                "search-filter",
            ],
            &["notes/2026-02-20.md:4"],
        ),
        item(
            "dark-theme",
            "Offer a dark colour scheme",
            [
                "easier on the eyes at night",
                "small",
                "low contrast on the muted labels",
                "the most asked-for change in the feedback form",
                "app/styles/theme.css",
                "",
            ],
            &["feedback/form-2026-02.csv:7"],
        ),
        item(
            "undo-delete",
            "Bring back an entry deleted by mistake",
            [
                "a slip of the finger no longer loses work",
                "medium",
                "the undo window must not outlive the session",
                "two support emails in February",
                "app/src/store.rs, app/src/undo.rs",
                "export-csv",
            ],
            &[
                "support/mail-0211.txt",
                "support/mail-0219.txt",
                "app/src/store.rs:88",
            ],
        ),
        item(
            "sync-retry",
            "Retry a failed sync before showing an error",
            [
                "fewer false alarms on a weak connection",
                "large",
                "a retry loop could drain the battery",
                "the error banner shows on most train journeys",
                "app/src/sync.rs",
                "dark-theme",
            ],
            &["logs/sync-2026-02-27.log:301"],
        ),
    ];
    assert_eq!(import.items, expected);
}

#[test]
fn notes_keep_their_kind_title_and_body_in_source_order() {
    let import = parse_backlog(BACKLOG).unwrap();
    let kinds: Vec<NoteKind> = import.notes.iter().map(|(kind, _, _)| *kind).collect();
    use NoteKind::{Held, History, LeftOut, Rule};
    assert_eq!(
        kinds,
        [
            Rule, Rule, History, History, History, History, Held, Held, Held, LeftOut, LeftOut,
            LeftOut
        ]
    );
    let (_, title, body) = &import.notes[1];
    assert_eq!(
        title,
        "Keep each item small enough to finish in one sitting; split anything"
    );
    assert_eq!(
        body,
        "Keep each item small enough to finish in one sitting; split anything\n\
         that needs more than one review round."
    );
    let (_, title, body) = &import.notes[4];
    assert_eq!(title, "Cleared items (2026-02-15, morning)");
    assert!(body.starts_with("- Removed two items that shipped:\n  ## 9. old-item:"));
    assert!(body.ends_with("release notes."));
    let (_, title, body) = &import.notes[10];
    assert_eq!(title, "calendar-view: looked at twice and set aside");
    assert!(body.contains("same thing\n\n  If the list grows"));
}

#[test]
fn a_long_first_line_is_cut_to_80_characters_for_the_title() {
    let line = "é".repeat(100);
    let import = parse_backlog(&format!("# Backlog: demo\n\n{line}\n")).unwrap();
    let (_, title, body) = &import.notes[0];
    assert_eq!(title.chars().count(), 80);
    assert_eq!(body, &line);
}

#[test]
fn the_backlog_export_keeps_every_line_in_order() {
    let import = parse_backlog(BACKLOG).unwrap();
    let export = export_backlog("demo", &ranked(&import), &numbered(&import));
    let source = content_lines(BACKLOG);
    let exported = content_lines(&export);
    println!(
        "content lines: source {}, export {}",
        source.len(),
        exported.len()
    );
    assert_eq!(source, exported);
    assert!(export.starts_with("# Backlog: demo\n"));
    // The export reads back to the same import.
    assert_eq!(parse_backlog(&export).unwrap(), import);
}

#[test]
fn the_queue_fixture_gives_its_counts_and_the_export_keeps_every_line() {
    let import = parse_queue(QUEUE).unwrap();
    assert_eq!(
        import.counts(),
        Counts {
            now: 1,
            parked: 4,
            done: 2,
            ..Counts::default()
        }
    );
    assert_eq!(
        import.date,
        Some(time::Date::from_calendar_date(2026, time::Month::March, 2).unwrap())
    );
    assert_eq!(
        import.now.as_deref(),
        Some(
            "Draft the release note for the filter change\n  \
             - list the three user-visible changes\n  \
             - link the issue for each one\n  \
             - ask for a read-through before posting"
        )
    );
    assert_eq!(
        import.parked[1],
        "Book the room for Thursday's review\n   - two hours, with a screen"
    );
    assert!(import.note.starts_with("Session started: 2026-03-02 09:15"));
    assert!(import.note.ends_with("before stopping."));
    let export = export_queue(&listed(&import));
    let source = content_lines(QUEUE);
    let exported = content_lines(&export);
    println!(
        "content lines: source {}, export {}",
        source.len(),
        exported.len()
    );
    assert_eq!(source, exported);
    assert_eq!(parse_queue(&export).unwrap(), import);
}

#[test]
fn an_empty_queue_exports_nothing_in_each_section() {
    let listing = QueueListing {
        now: None,
        parked: Vec::new(),
        done: Vec::new(),
        note: String::new(),
    };
    let export = export_queue(&listing);
    assert_eq!(
        export,
        "# Queue\n\n## Now\n\nNothing.\n\n## Parked\n\nNothing.\n\n## Done\n\nNothing.\n"
    );
    let import = parse_queue(&export).unwrap();
    assert_eq!(import.counts(), Counts::default());
    assert_eq!(import.date, None);
}

#[test]
fn a_line_it_cannot_place_is_unparsed_line_n() {
    let (text, k) = insert_after(BACKLOG, "- size:", "Some loose text.");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
    let (text, k) = insert_after(BACKLOG, "- emoji-picker", "## Undated heading");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
    let (text, k) = insert_after(QUEUE, "3. Tidy", "A stray paragraph.");
    assert_eq!(parse_queue(&text), Err(ParseError::Unparsed(k)));
    assert_eq!(
        ParseError::Unparsed(k).to_string(),
        format!("unparsed line {k}")
    );
}

#[test]
fn other_lines_out_of_place_are_unparsed() {
    // An indented line with no field to continue.
    let (text, k) = insert_after(BACKLOG, "## 3. dark-theme", "  hanging");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
    // A second Now bullet.
    let (text, k) = insert_after(QUEUE, "  - ask for", "- A second now");
    assert_eq!(parse_queue(&text), Err(ParseError::Unparsed(k)));
    // A heading the queue does not know.
    let (text, k) = insert_after(QUEUE, "- Answered", "## Later");
    assert_eq!(parse_queue(&text), Err(ParseError::Unparsed(k)));
    // A second title line.
    let (text, k) = insert_after(BACKLOG, "- tags:", "# Backlog: again");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
}

#[test]
fn a_field_out_of_order_or_repeated_is_unparsed() {
    let (text, k) = insert_after(BACKLOG, "- risk: the filter", "- size: large");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
    let (text, k) = insert_after(BACKLOG, "- risk: the filter", "- risk: again");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
    let (text, k) = insert_after(BACKLOG, "- risk: the filter", "- colour: blue");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
    // A field with no value would be left out of the export, so it is refused.
    let (text, k) = insert_after(BACKLOG, "## 3. dark-theme", "- value:");
    assert_eq!(parse_backlog(&text), Err(ParseError::Unparsed(k)));
}

#[test]
fn an_item_that_fails_its_limits_is_invalid() {
    let text = "## 1. a b: Title\n- evidence: x\n";
    assert_eq!(parse_backlog(text), Err(ParseError::Invalid("id".into())));
    let text = "## 1. a: Title\n- size: huge\n- evidence: x\n";
    assert_eq!(parse_backlog(text), Err(ParseError::Invalid("size".into())));
    let text = "## 1. a: Title\n- value: v\n";
    assert_eq!(
        parse_backlog(text),
        Err(ParseError::Invalid("evidence".into()))
    );
    // A repeated id would replace the first item in the store.
    let text = "## 1. a: One\n- evidence: x\n\n## 2. a: Two\n- evidence: y\n";
    let refused = parse_backlog(text).unwrap_err();
    assert_eq!(refused, ParseError::Invalid("id".into()));
    assert_eq!(refused.to_string(), "invalid id");
}

#[test]
fn a_sixth_item_is_backlog_full() {
    let text: String = (1..=6)
        .map(|n| format!("## {n}. item-{n}: Item {n}\n- evidence: e{n}\n\n"))
        .collect();
    let refused = parse_backlog(&text).unwrap_err();
    assert_eq!(refused, ParseError::Full);
    assert_eq!(refused.to_string(), "backlog_full");
}

#[test]
fn crlf_reads_the_same() {
    let backlog = BACKLOG.replace('\n', "\r\n");
    let queue = QUEUE.replace('\n', "\r\n");
    let lf = parse_backlog(BACKLOG).unwrap();
    let crlf = parse_backlog(&backlog).unwrap();
    assert_eq!(crlf.counts(), lf.counts());
    assert_eq!(crlf, lf);
    assert_eq!(
        export_backlog("demo", &ranked(&crlf), &numbered(&crlf)),
        export_backlog("demo", &ranked(&lf), &numbered(&lf))
    );
    let lf = parse_queue(QUEUE).unwrap();
    let crlf = parse_queue(&queue).unwrap();
    assert_eq!(crlf.counts(), lf.counts());
    assert_eq!(crlf, lf);
    assert_eq!(export_queue(&listed(&crlf)), export_queue(&listed(&lf)));
    assert_eq!(content_lines(&backlog), content_lines(BACKLOG));
}
