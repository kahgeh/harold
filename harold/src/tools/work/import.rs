//! The import: the chief's work files, and the queue items that the chief
//! keeps. The folder and the queue are only read. A dry run stores nothing,
//! and an applied import stores all the records in one transaction, or none.

use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harold_api::tools::backlog::v1 as bp;
use harold_api::tools::backlog::v1::backlog_service_client::BacklogServiceClient;

use super::rotation::Rotator;
use super::schema::{Decision, Mark, Priority, Record, Status, valid_link, valid_name};
use super::store::{Refusal, StoreError};
use crate::tools::BoxFuture;

/// The largest chief's file that is read, in bytes.
pub(super) const FILE_MAX: u64 = 64 * 1024;

/// The most `*.md` files in one folder.
pub(super) const FILES_MAX: usize = 200;

/// The longest wait for the backlog tool: for the connection, and for the call.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(2);

/// A line of a chief's file that fits no field. Line 0 is the whole file, for
/// a file that is not read or that has no heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Unplaced {
    pub(super) file: String,
    /// From 1. 0 is the whole file.
    pub(super) line: u32,
    /// Why the line is not placed: one fixed snake_case word, such as
    /// `bad_status`. The parser sets it where it decides.
    pub(super) reason: &'static str,
    /// The text of the line. Empty for a whole file, except a name in use.
    pub(super) text: String,
}

/// One chief's file, parsed. `record` is `None` when the file has no heading,
/// no priority or no status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedFile {
    pub(super) record: Option<Record>,
    pub(super) unplaced: Vec<Unplaced>,
}

/// An open queue item: the `now` item or a parked one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueueItem {
    pub(super) id: String,
    pub(super) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueueUnreadable;

/// The queue's open items. It only reads.
pub(super) trait OpenQueue: Send + Sync {
    fn open_items(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, QueueUnreadable>>;
}

/// Reads the queue through the backlog tool's `ListQueue` call on Harold's own
/// loopback address, as the CLI does. It never writes the queue.
pub(super) struct LoopbackQueue {
    address: Option<SocketAddr>,
}

impl LoopbackQueue {
    pub(super) fn new(address: Option<SocketAddr>) -> Self {
        Self { address }
    }
}

impl OpenQueue for LoopbackQueue {
    fn open_items(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, QueueUnreadable>> {
        Box::pin(async move {
            let address = self.address.ok_or(QueueUnreadable)?;
            let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .map_err(|_| QueueUnreadable)?
                .connect_timeout(QUEUE_TIMEOUT)
                .timeout(QUEUE_TIMEOUT);
            let channel = tokio::time::timeout(QUEUE_TIMEOUT, endpoint.connect())
                .await
                .map_err(|_| QueueUnreadable)?
                .map_err(|_| QueueUnreadable)?;
            let mut client = BacklogServiceClient::new(channel);
            // No done item is wanted, so none is asked for.
            let call = client.list_queue(bp::ListQueueRequest {
                done_since_ms: i64::MAX,
            });
            let listing = tokio::time::timeout(QUEUE_TIMEOUT, call)
                .await
                .map_err(|_| QueueUnreadable)?
                .map_err(|_| QueueUnreadable)?
                .into_inner();
            Ok(listing
                .now
                .into_iter()
                .chain(listing.parked)
                .map(|item| QueueItem {
                    id: item.id,
                    text: item.text,
                })
                .collect())
        })
    }
}

pub(super) struct ImportDeps<'a> {
    pub(super) rotator: &'a Rotator,
    pub(super) queue: &'a dyn OpenQueue,
    pub(super) now_ms: i64,
}

/// What is stored, or in a dry run what would be stored, with every unplaced
/// line and every name that the current work file holds already.
#[derive(Debug, Default)]
pub(super) struct ImportReport {
    pub(super) records: Vec<Record>,
    pub(super) unplaced: Vec<Unplaced>,
    pub(super) in_use: Vec<String>,
}

/// Why an import was refused. Nothing is stored then.
#[derive(Debug)]
pub(super) enum ImportError {
    /// The folder is not an absolute path to a folder that can be read.
    BadFolder,
    /// The folder holds more than `FILES_MAX` `*.md` files.
    TooManyFiles,
    /// An empty queue id.
    BadQueueId,
    QueueUnreadable,
    /// The queue item's id does not make a valid record name.
    BadQueueName(String),
    /// The id is not the `now` item or a parked item of the queue.
    NotOpenQueueItem(String),
    /// `--apply` with this many unplaced lines.
    Unplaced(usize),
    /// `--apply` with these names in use in the current work file.
    NamesInUse(Vec<String>),
    /// `--apply` would put a third open work record in focus.
    FocusFull(String),
    /// The current work file is old and refuses writes.
    ReadOnly,
    Store(StoreError),
}

/// Parses one chief's file. The heading `# <name>: <title>` gives the name and
/// the title (`—`, `–` or `-` may take the place of `:`). Then come
/// `Priority:`, `Status:`, `Waits for:`, `Next step:`, `Decisions:` with
/// `- <mark>, <who>, <date>: <text>` lines, and `Links:` with `- <link>`
/// lines. Blank lines are skipped. Every other line is unplaced.
pub(super) fn parse_chief_file(file_name: &str, text: &str) -> ParsedFile {
    let mut parser = Parser::new(file_name);
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // `lines` drops a `\r` before each `\n`.
    for (index, raw) in text.lines().enumerate() {
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        parser.line(number, raw.trim_end_matches('\r'));
    }
    parser.finish()
}

/// A queue item as a work record: the name `queue-<id>` lower-cased, status
/// `idea`, priority `later`, and the first line of its text as the next step.
pub(super) fn queue_record(id: &str, text: &str) -> Record {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    Record {
        name: format!("queue-{}", id.to_ascii_lowercase()),
        title: String::new(),
        priority: Priority::Later,
        status: Status::Idea,
        waits_for: String::new(),
        next_step: first.to_string(),
        created_in: String::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
        status_since_ms: 0,
        decisions: Vec::new(),
        links: Vec::new(),
    }
}

/// Reads the `*.md` files directly inside `folder` and the queue items named
/// in `queue_ids`. Without `apply`, it stores nothing. With `apply`, it
/// refuses while a line is unplaced or a name is in use, and otherwise
/// stores every record through the current work file in one transaction.
pub(super) async fn import(
    folder: &Path,
    queue_ids: &[String],
    apply: bool,
    deps: &ImportDeps<'_>,
) -> Result<ImportReport, ImportError> {
    let wanted = wanted_ids(queue_ids)?;
    let folder = folder.to_path_buf();
    let files = tokio::task::spawn_blocking(move || read_folder(&folder))
        .await
        .map_err(|_| ImportError::BadFolder)??;

    let mut report = ImportReport::default();
    let mut names = HashSet::new();
    for (file, content) in files {
        let text = match content {
            Ok(text) => text,
            Err(problem) => {
                report.unplaced.push(Unplaced {
                    file,
                    line: 0,
                    reason: problem,
                    text: String::new(),
                });
                continue;
            }
        };
        let parsed = parse_chief_file(&file, &text);
        report.unplaced.extend(parsed.unplaced);
        if let Some(record) = parsed.record {
            match names.insert(record.name.clone()) {
                true => report.records.push(record),
                false => report.unplaced.push(Unplaced {
                    reason: "name_used_by_another_file",
                    text: record.name,
                    file,
                    line: 0,
                }),
            }
        }
    }

    if !wanted.is_empty() {
        let open = deps
            .queue
            .open_items()
            .await
            .map_err(|_| ImportError::QueueUnreadable)?;
        for id in wanted {
            let item = open
                .iter()
                .find(|item| item.id.eq_ignore_ascii_case(&id))
                .ok_or_else(|| ImportError::NotOpenQueueItem(id.clone()))?;
            let record = queue_record(&item.id, &item.text);
            if !valid_name(&record.name) {
                return Err(ImportError::BadQueueName(item.id.clone()));
            }
            match names.insert(record.name.clone()) {
                true => report.records.push(record),
                false => report.unplaced.push(Unplaced {
                    reason: "name_used_by_another_file",
                    text: record.name,
                    file: format!("queue {}", item.id),
                    line: 0,
                }),
            }
        }
    }

    if apply && !report.unplaced.is_empty() {
        return Err(ImportError::Unplaced(report.unplaced.len()));
    }
    let file = deps.rotator.current().await.map_err(ImportError::Store)?;
    for record in &report.records {
        if file
            .get(&record.name)
            .await
            .map_err(ImportError::Store)?
            .is_some()
        {
            report.in_use.push(record.name.clone());
        }
    }
    if !apply {
        return Ok(report);
    }
    if !report.in_use.is_empty() {
        return Err(ImportError::NamesInUse(report.in_use));
    }
    match file
        .put_new_all(&report.records, deps.now_ms)
        .await
        .map_err(ImportError::Store)?
    {
        Ok(()) => Ok(report),
        Err((Refusal::NameInUse, name)) => Err(ImportError::NamesInUse(vec![name])),
        Err((Refusal::FocusFull, name)) => Err(ImportError::FocusFull(name)),
        Err((Refusal::ReadOnly, _)) => Err(ImportError::ReadOnly),
        // The parser and `queue_record` give only valid names, and a new
        // record is never `NotFound`.
        Err((refusal, name)) => Err(ImportError::Store(StoreError::Data(format!(
            "{} for {name}",
            refusal.reason()
        )))),
    }
}

/// The queue ids, each once, in the order given.
fn wanted_ids(queue_ids: &[String]) -> Result<Vec<String>, ImportError> {
    let mut seen = BTreeSet::new();
    let mut wanted = Vec::new();
    for id in queue_ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(ImportError::BadQueueId);
        }
        if seen.insert(id.to_ascii_lowercase()) {
            wanted.push(id.to_string());
        }
    }
    Ok(wanted)
}

/// The text of each `*.md` file directly inside `folder`, by file name. A file
/// that cannot be read gives the reason in place of its text. Hidden files,
/// other files and sub-folders are skipped.
type FolderFiles = Vec<(String, Result<String, &'static str>)>;

fn read_folder(folder: &Path) -> Result<FolderFiles, ImportError> {
    if !folder.is_absolute() {
        return Err(ImportError::BadFolder);
    }
    let entries = std::fs::read_dir(folder).map_err(|_| ImportError::BadFolder)?;
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    // Files the import does not read. They are listed, so `--apply` refuses.
    let mut hidden_or_upper: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ImportError::BadFolder)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_ascii_lowercase().ends_with(".md") {
            continue;
        }
        if name.starts_with('.') || !name.ends_with(".md") {
            hidden_or_upper.push(name);
            continue;
        }
        paths.push((name, entry.path()));
        if paths.len() > FILES_MAX {
            return Err(ImportError::TooManyFiles);
        }
    }
    paths.sort();
    let mut files: FolderFiles = paths
        .into_iter()
        .map(|(name, path)| {
            let text = read_file(&path);
            (name, text)
        })
        .collect();
    hidden_or_upper.sort();
    files.extend(
        hidden_or_upper
            .into_iter()
            .map(|name| (name, Err("hidden_or_upper_case_name"))),
    );
    Ok(files)
}

/// A plain file of at most `FILE_MAX` bytes of UTF-8 text. A link is not
/// followed.
fn read_file(path: &Path) -> Result<String, &'static str> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| "unreadable_file")?;
    if !meta.is_file() {
        return Err("not_a_file");
    }
    let file = std::fs::File::open(path).map_err(|_| "unreadable_file")?;
    let mut bytes = Vec::new();
    file.take(FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unreadable_file")?;
    if bytes.len() as u64 > FILE_MAX {
        return Err("file_too_large");
    }
    String::from_utf8(bytes).map_err(|_| "not_utf8")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Fields,
    Decisions,
    Links,
}

struct Parser<'a> {
    file: &'a str,
    /// The line number, the name, the title and the line's text.
    heading: Option<(u32, String, String, String)>,
    priority: Option<Priority>,
    status: Option<Status>,
    waits_for: Option<String>,
    next_step: Option<String>,
    decisions_seen: bool,
    links_seen: bool,
    decisions: Vec<Decision>,
    links: Vec<String>,
    section: Section,
    unplaced: Vec<Unplaced>,
}

impl<'a> Parser<'a> {
    fn new(file: &'a str) -> Self {
        Self {
            file,
            heading: None,
            priority: None,
            status: None,
            waits_for: None,
            next_step: None,
            decisions_seen: false,
            links_seen: false,
            decisions: Vec::new(),
            links: Vec::new(),
            section: Section::Fields,
            unplaced: Vec::new(),
        }
    }

    fn unplaced(&mut self, line: u32, reason: &'static str, text: &str) {
        self.unplaced.push(Unplaced {
            file: self.file.to_string(),
            line,
            reason,
            text: text.to_string(),
        });
    }

    fn line(&mut self, number: u32, raw: &str) {
        let line = raw.trim();
        if line.is_empty() {
            return;
        }
        if self.heading.is_none() {
            match line.strip_prefix("# ") {
                Some(rest) => match heading(rest) {
                    Some((name, title)) => {
                        self.heading = Some((number, name, title, raw.to_string()))
                    }
                    None => self.unplaced(number, "bad_heading", raw),
                },
                None => self.unplaced(number, "no_heading", raw),
            }
            return;
        }
        let placed = match self.field(line) {
            Some(placed) => placed,
            None => self.item(line),
        };
        if let Err(reason) = placed {
            self.unplaced(number, reason, raw);
        }
    }

    /// `None` when the line is not a field line. `Some(Err(reason))` when it
    /// is a field line that cannot be placed: a bad value or a second one.
    fn field(&mut self, line: &str) -> Option<Result<(), &'static str>> {
        if line.starts_with("# ") {
            return Some(Err("second_heading"));
        }
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        let placed = match key {
            "Priority" => set_once(
                &mut self.priority,
                Priority::parse(&value.to_ascii_lowercase()),
                "bad_priority",
            ),
            "Status" => set_once(
                &mut self.status,
                Status::parse(&value.to_ascii_lowercase()),
                "bad_status",
            ),
            "Waits for" => set_once(&mut self.waits_for, Some(value.to_string()), ""),
            "Next step" => set_once(&mut self.next_step, Some(value.to_string()), ""),
            "Decisions" => {
                let first = !std::mem::replace(&mut self.decisions_seen, true);
                self.section = Section::Decisions;
                section_head(first, value)
            }
            "Links" => {
                let first = !std::mem::replace(&mut self.links_seen, true);
                self.section = Section::Links;
                section_head(first, value)
            }
            _ => return None,
        };
        if !matches!(key, "Decisions" | "Links") {
            self.section = Section::Fields;
        }
        Some(placed)
    }

    /// A `- ` line of the decisions or the links.
    fn item(&mut self, line: &str) -> Result<(), &'static str> {
        let Some(item) = line.strip_prefix("- ").map(str::trim) else {
            return Err("unknown_line");
        };
        match self.section {
            Section::Fields => Err("item_outside_section"),
            Section::Decisions => {
                self.decisions.push(decision(item).ok_or("bad_decision")?);
                Ok(())
            }
            Section::Links => match valid_link(item) {
                true => {
                    self.links.push(item.to_string());
                    Ok(())
                }
                false => Err("bad_link"),
            },
        }
    }

    fn finish(mut self) -> ParsedFile {
        let Some((line, name, title, heading_text)) = self.heading.take() else {
            self.unplaced(0, "no_heading", "");
            return ParsedFile {
                record: None,
                unplaced: self.unplaced,
            };
        };
        if self.priority.is_none() {
            self.unplaced(line, "missing_priority", &heading_text);
        }
        if self.status.is_none() {
            self.unplaced(line, "missing_status", &heading_text);
        }
        let record = match (self.priority, self.status) {
            (Some(priority), Some(status)) => Some(Record {
                name,
                title,
                priority,
                status,
                waits_for: self.waits_for.unwrap_or_default(),
                next_step: self.next_step.unwrap_or_default(),
                created_in: String::new(),
                created_at_ms: 0,
                updated_at_ms: 0,
                status_since_ms: 0,
                decisions: self.decisions,
                links: self.links,
            }),
            _ => None,
        };
        ParsedFile {
            record,
            unplaced: self.unplaced,
        }
    }
}

/// Sets `slot` once. A second value gives `second_value`, a bad one gives
/// `bad_value`.
fn set_once<T>(
    slot: &mut Option<T>,
    value: Option<T>,
    bad_value: &'static str,
) -> Result<(), &'static str> {
    match (slot.is_some(), value) {
        (true, _) => Err("second_value"),
        (false, None) => Err(bad_value),
        (false, Some(value)) => {
            *slot = Some(value);
            Ok(())
        }
    }
}

/// The `Decisions:` or `Links:` line: it has no value, and it is the first.
fn section_head(first: bool, value: &str) -> Result<(), &'static str> {
    match (first, value.is_empty()) {
        (_, false) => Err("inline_value_after_section"),
        (false, true) => Err("second_value"),
        (true, true) => Ok(()),
    }
}

/// `<name>: <title>`, or with `—`, `–` or `-` in place of `:`. The title may be
/// empty. A name that is not valid gives `None`.
fn heading(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    let end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    let (name, after) = rest.split_at(end);
    if !valid_name(name) {
        return None;
    }
    let after = after.trim_start();
    let title = match after {
        "" => "",
        _ => after
            .strip_prefix(':')
            .or_else(|| after.strip_prefix('—'))
            .or_else(|| after.strip_prefix('–'))
            .or_else(|| after.strip_prefix('-'))?,
    };
    Some((name.to_string(), title.trim().to_string()))
}

/// `<mark>, <who>, <date>: <text>`, with one of the four marks, a date
/// `YYYY-MM-DD`, and a `who` and a text that are not empty.
fn decision(item: &str) -> Option<Decision> {
    let (mark, rest) = item.split_once(',')?;
    let mark = Mark::parse(&mark.trim().to_ascii_lowercase())?;
    let (head, text) = rest.split_once(':')?;
    let (who, at) = head.rsplit_once(',')?;
    let (who, at, text) = (who.trim(), at.trim(), text.trim());
    if who.is_empty() || text.is_empty() || !is_date(at) {
        return None;
    }
    Some(Decision {
        at: at.to_string(),
        who: who.to_string(),
        mark,
        text: text.to_string(),
    })
}

/// `YYYY-MM-DD`, with a month from 01 to 12 and a day from 01 to 31.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| {
        text.get(range.clone())
            .filter(|part| part.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|part| part.parse::<u32>().ok())
    };
    matches!(
        (digits(0..4), digits(5..7), digits(8..10)),
        (Some(_), Some(1..=12), Some(1..=31))
    )
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
