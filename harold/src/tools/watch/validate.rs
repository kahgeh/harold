//! Checking a `StartWatch` request, and finding and reading a watch's skill. The watch
//! tool trusts local callers (ADR 0002) but not their values: every field is checked
//! here, and the skill is looked up again, with the same rule, at each run.

use std::path::{Path, PathBuf};

use harold_api::tools::watch::v1::StartWatchRequest;

use super::runner::RUN_LIMIT;
use crate::settings::WatchSettings;

const MAX_NAME_LEN: usize = 64;
const EVERY_MINUTES: std::ops::RangeInclusive<u32> = 2..=240;
const FOR_MINUTES: std::ops::RangeInclusive<u32> = 1..=1440;
const MIN_RUN_BUDGET_MICROS: i64 = 10_000;
/// A `SKILL.md` larger than this is treated as missing: it would not fit a model prompt.
const MAX_SKILL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReportTo {
    Chief,
    /// The agent running in the watch's own folder.
    SelfPane,
}

impl ReportTo {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ReportTo::Chief => "chief",
            ReportTo::SelfPane => "self",
        }
    }
}

/// A start request that passed every check.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Checked {
    pub name: String,
    /// The canonical folder: links followed, no trailing slash.
    pub folder: PathBuf,
    pub skill: String,
    pub report_to: ReportTo,
    pub every_minutes: u32,
    pub for_minutes: u32,
    pub every_run: bool,
    /// `None` when `start` gave none and `[watch] run_budget_usd` is not set.
    pub run_budget_usd_micros: Option<i64>,
    /// The most seconds one run may take: 120 when `start` gave none.
    pub run_limit_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The field named in `invalid <field>`.
    Invalid(&'static str),
    /// `run_limit_secs` is above `[watch] max_run_limit_secs`, which is the value.
    RunLimitAboveMax(u32),
    UnknownSkill,
}

/// A skill read from `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Skill {
    /// The frontmatter's `allowed-tools`, exactly as written, or `None` when absent.
    pub allowed_tools: Option<String>,
    /// `allowed-tools:` written as a YAML list, which is not read: `allowed_tools` stays `None`.
    pub tools_unread: bool,
    /// The frontmatter's `add-dirs`: the extra folders a run may read, `~` expanded.
    pub add_dirs: Vec<String>,
    /// The text after the frontmatter.
    pub body: String,
}

/// The skill's file is not there, leaves `.claude/skills/`, or cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SkillMissing;

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_NAME_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Checks every field of a start request. `UnknownSkill` is last: a skill is looked for
/// only when everything else is valid.
pub(super) fn check_start(
    request: &StartWatchRequest,
    cfg: &WatchSettings,
) -> Result<Checked, Refusal> {
    if !is_token(&request.name) {
        return Err(Refusal::Invalid("name"));
    }
    let folder = canonical_folder(&request.folder).ok_or(Refusal::Invalid("folder"))?;
    if !is_token(&request.skill) {
        return Err(Refusal::Invalid("skill"));
    }
    if !EVERY_MINUTES.contains(&request.every_minutes) {
        return Err(Refusal::Invalid("every_minutes"));
    }
    if !FOR_MINUTES.contains(&request.for_minutes) {
        return Err(Refusal::Invalid("for_minutes"));
    }
    let report_to = match request.report_to.as_str() {
        "chief" => ReportTo::Chief,
        "" | "self" => ReportTo::SelfPane,
        _ => return Err(Refusal::Invalid("report_to")),
    };
    let run_budget_usd_micros = run_budget(request.run_budget_usd, cfg)?;
    let run_limit_secs = run_limit(request.run_limit_secs, cfg)?;
    skill_path(&folder, &request.skill, &cfg.skill_folders).ok_or(Refusal::UnknownSkill)?;
    Ok(Checked {
        name: request.name.clone(),
        folder,
        skill: request.skill.clone(),
        report_to,
        every_minutes: request.every_minutes,
        for_minutes: request.for_minutes,
        every_run: request.every_run,
        run_budget_usd_micros,
        run_limit_secs,
    })
}

/// The run limit in seconds. 0 means none given, so the default applies.
fn run_limit(asked: u32, cfg: &WatchSettings) -> Result<u32, Refusal> {
    if asked == 0 {
        return Ok(RUN_LIMIT.as_secs() as u32);
    }
    match cfg.max_run_limit_secs {
        Some(max) if asked > max => Err(Refusal::RunLimitAboveMax(max)),
        _ => Ok(asked),
    }
}

/// The run budget in millionths of a dollar. 0 means none given, so the setting applies
/// when it is set.
fn run_budget(asked: f64, cfg: &WatchSettings) -> Result<Option<i64>, Refusal> {
    const INVALID: Refusal = Refusal::Invalid("run_budget_usd");
    if asked == 0.0 {
        return Ok(cfg.run_budget_usd.map(to_micros));
    }
    if !asked.is_finite() || asked < 0.0 {
        return Err(INVALID);
    }
    let micros = to_micros(asked);
    if micros < MIN_RUN_BUDGET_MICROS
        || cfg
            .max_run_budget_usd
            .is_some_and(|max| micros > to_micros(max))
    {
        return Err(INVALID);
    }
    Ok(Some(micros))
}

pub(super) fn to_micros(usd: f64) -> i64 {
    (usd * 1_000_000.0).round() as i64
}

/// The canonical form of an absolute folder that exists, else `None`.
fn canonical_folder(folder: &str) -> Option<PathBuf> {
    let path = Path::new(folder);
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    canonical.is_dir().then_some(canonical)
}

/// `<folder>/.claude/skills/<skill>/SKILL.md`, if it is a file whose real path lies inside
/// `<folder>/.claude/skills/` or inside one of the user's `skill_folders`. Paths are
/// compared part by part, so `/x/watches-old/a` is not inside `/x/watches`, and each root
/// is compared by its real path, so a listed folder that is itself a link counts as what
/// it points to. A root that does not exist matches nothing.
fn skill_path(folder: &Path, skill: &str, skill_folders: &[String]) -> Option<PathBuf> {
    if !is_token(skill) {
        return None;
    }
    let skills = folder.join(".claude").join("skills");
    let file = std::fs::canonicalize(skills.join(skill).join("SKILL.md")).ok()?;
    let own = std::fs::canonicalize(&skills).ok();
    let listed = skill_folders
        .iter()
        .filter_map(|listed| std::fs::canonicalize(listed).ok());
    let inside = own
        .into_iter()
        .chain(listed)
        .any(|root| file.starts_with(&root));
    (inside && file.is_file()).then_some(file)
}

/// Reads the skill afresh. The same location rule as `check_start`, so a link that
/// was changed after the watch started is caught at the run.
pub(super) fn read_skill(
    folder: &Path,
    skill: &str,
    skill_folders: &[String],
) -> Result<Skill, SkillMissing> {
    let path = skill_path(folder, skill, skill_folders).ok_or(SkillMissing)?;
    let size = std::fs::metadata(&path).map_err(|_| SkillMissing)?.len();
    if size > MAX_SKILL_BYTES {
        return Err(SkillMissing);
    }
    let text = std::fs::read_to_string(&path).map_err(|_| SkillMissing)?;
    Ok(parse_skill(&text, std::env::var("HOME").ok().as_deref()))
}

/// A leading `~` or `~/` becomes `home`; every other entry, and any entry when `home` is
/// not known, goes as written.
fn expand_home(entry: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return entry.to_owned();
    };
    if entry == "~" {
        return home.to_owned();
    }
    match entry.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None => entry.to_owned(),
    }
}

/// A YAML list item: `-` then a space or the end of the line, so the closing `---` is not one.
fn is_list_item(line: &str) -> bool {
    line.trim()
        .strip_prefix('-')
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn parse_skill(text: &str, home: Option<&str>) -> Skill {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Skill {
            allowed_tools: None,
            tools_unread: false,
            add_dirs: Vec::new(),
            body: text.to_owned(),
        };
    };
    let mut allowed_tools = None;
    let mut tools_unread = false;
    let mut add_dirs = Vec::new();
    let mut consumed = 0;
    let mut closed = false;
    let mut lines = rest.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        consumed += line.len();
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare == "---" {
            closed = true;
            break;
        }
        if let Some(value) = bare.strip_prefix("allowed-tools:") {
            let value = value.trim();
            if !value.is_empty() {
                allowed_tools = Some(value.to_owned());
            } else if lines.peek().is_some_and(|next| is_list_item(next)) {
                tools_unread = true;
            }
        }
        if let Some(value) = bare.strip_prefix("add-dirs:") {
            add_dirs = value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| expand_home(entry, home))
                .collect();
        }
    }
    if !closed {
        // Frontmatter that never closes is not frontmatter: the whole text is the body.
        return Skill {
            allowed_tools: None,
            tools_unread: false,
            add_dirs: Vec::new(),
            body: text.to_owned(),
        };
    }
    Skill {
        allowed_tools,
        tools_unread,
        add_dirs,
        body: rest[consumed..].to_owned(),
    }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
