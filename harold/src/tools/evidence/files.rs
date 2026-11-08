//! The evidence file `evidence/<id>.md` and the confirmed file `confirmed/<id>.md` in the
//! pod folder. Each write goes to a temporary name and then renames, so a reader never
//! sees half a file.

use std::io;
use std::path::Path;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::state::{QueryResult, State};

/// The keys that the evidence tool writes itself. Any other `key: value` line of an old
/// file, such as `told:`, belongs to a later step and survives a rewrite.
const OWN_KEYS: [&str; 5] = ["checked", "since", "released", "state", "query"];

/// The content of `evidence/<id>.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EvidenceFile {
    pub checked: OffsetDateTime,
    pub since: OffsetDateTime,
    /// None while no tag holds the merge.
    pub released: Option<OffsetDateTime>,
    pub state: State,
    pub results: Vec<QueryResult>,
    /// The lines that a later step added, such as `told:`, as they stand.
    pub extra_lines: Vec<String>,
}

fn time_text(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap_or_default()
}

fn render(id: &str, file: &EvidenceFile) -> String {
    let released = file.released.map(time_text).unwrap_or_default();
    let mut text = format!(
        "# Evidence: {id}\n\nchecked: {}\nsince: {}\nreleased:{}{released}\nstate: {}\n",
        time_text(file.checked),
        time_text(file.since),
        if released.is_empty() { "" } else { " " },
        file.state.as_str(),
    );
    for result in &file.results {
        let word = if result.present { "present" } else { "absent" };
        text.push_str(&format!(
            "query: {} {word} {}\n",
            result.name, result.reason
        ));
    }
    for line in &file.extra_lines {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// Parses the text of an evidence file. None when a required line is missing or wrong.
fn parse(text: &str) -> Option<EvidenceFile> {
    let (mut checked, mut since, mut released, mut state) = (None, None, None, None);
    let mut results = Vec::new();
    let mut extra_lines = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let time = || OffsetDateTime::parse(value, &Rfc3339).ok();
        match key {
            "checked" => checked = Some(time()?),
            "since" => since = Some(time()?),
            "released" => {
                released = Some(if value.is_empty() {
                    None
                } else {
                    Some(time()?)
                })
            }
            "state" => state = Some(State::parse(value)?),
            "query" => {
                let mut words = value.split_whitespace();
                let name = words.next()?.to_string();
                let present = match words.next()? {
                    "present" => true,
                    "absent" => false,
                    _ => return None,
                };
                let reason = words.next()?.to_string();
                results.push(QueryResult {
                    name,
                    present,
                    reason,
                });
            }
            _ if OWN_KEYS.contains(&key) || key.starts_with('#') => {}
            _ => extra_lines.push(line.to_string()),
        }
    }
    Some(EvidenceFile {
        checked: checked?,
        since: since?,
        released: released?,
        state: state?,
        results,
        extra_lines,
    })
}

/// Reads `evidence/<id>.md`. None when the file is absent or does not parse.
pub(crate) fn read_evidence(pod: &Path, id: &str) -> Option<EvidenceFile> {
    let text = std::fs::read_to_string(pod.join("evidence").join(format!("{id}.md"))).ok()?;
    parse(&text)
}

/// Writes `evidence/<id>.md`. The lines of the old file that the tool does not own, such as
/// `told:`, stay at the end of the new one.
pub(crate) fn write_evidence(pod: &Path, id: &str, file: &EvidenceFile) -> io::Result<()> {
    let mut file = file.clone();
    if let Some(old) = read_evidence(pod, id) {
        file.extra_lines = old.extra_lines;
    }
    write_atomic(&pod.join("evidence"), id, &render(id, &file))
}

/// Writes `confirmed/<id>.md` unless it exists. Returns true when it wrote the file.
pub(crate) fn write_confirmed_once(
    pod: &Path,
    id: &str,
    at: OffsetDateTime,
    queries: &[String],
) -> io::Result<bool> {
    let folder = pod.join("confirmed");
    if folder.join(format!("{id}.md")).exists() {
        return Ok(false);
    }
    let text = format!(
        "# Confirmed: {id}\n\nconfirmed: {}\nevidence: {}\n",
        time_text(at),
        queries.join(" ")
    );
    write_atomic(&folder, id, &text)?;
    Ok(true)
}

fn write_atomic(folder: &Path, id: &str, text: &str) -> io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let temporary = folder.join(format!(".{id}.md.tmp"));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, folder.join(format!("{id}.md")))
}
