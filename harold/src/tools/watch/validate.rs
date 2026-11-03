//! Checking a `StartWatch` request, and finding and reading a watch's skill. The watch
//! tool trusts local callers (ADR 0002) but not their values: every field is checked
//! here, and the skill is looked up again, with the same rule, at each run.

use std::path::{Path, PathBuf};

use harold_api::tools::watch::v1::remind_request::When;
use harold_api::tools::watch::v1::{PauseRequest, RemindRequest, ResumeRequest, StartWatchRequest};

use super::runner::RUN_LIMIT;
use crate::agent::summary::sanitize_bounded_metadata;
use crate::cron::{Cron, CronError};
use crate::settings::WatchSettings;
use crate::timed_prompts::LocalNow;

const MAX_NAME_LEN: usize = 64;
const EVERY_MINUTES: std::ops::RangeInclusive<u32> = 2..=240;
/// The smallest gap between two runs of a cron watch: the `--every` lower limit.
const MIN_CRON_GAP_MINUTES: u32 = 2;
const FOR_MINUTES: std::ops::RangeInclusive<u32> = 1..=1440;
/// The most characters of a reminder's text.
const MAX_TEXT_CHARS: usize = 300;
const IN_MINUTES: std::ops::RangeInclusive<u32> = 1..=1440;
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
    /// 0 for a cron watch.
    pub every_minutes: u32,
    /// The cron expression, or `None` for a watch at an interval.
    pub cron: Option<Cron>,
    /// `None` when `start` gave none: the watch has no end.
    pub for_minutes: Option<u32>,
    pub every_run: bool,
    /// `None` when `start` gave none and `[watch] run_budget_usd` is not set.
    pub run_budget_usd_micros: Option<i64>,
    /// The most seconds one run may take: 120 when `start` gave none.
    pub run_limit_secs: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Refusal {
    /// The field named in `invalid <field>`.
    Invalid(&'static str),
    /// `run_limit_secs` is above `[watch] max_run_limit_secs`, which is the value.
    RunLimitAboveMax(u32),
    /// The run budget is above `[watch] daily_budget_usd`, which is the value in dollars.
    RunBudgetAboveDaily(f64),
    UnknownSkill,
    /// `start` gave both `every_minutes` and `cron`, or neither.
    EveryOrCron,
    /// A cron expression that does not parse, or that never runs.
    Cron(CronError),
    /// A cron watch with two runs less than 2 minutes apart.
    TooOften,
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
    let (every_minutes, cron) = match (request.every_minutes, request.cron.as_deref()) {
        (Some(minutes), None) if EVERY_MINUTES.contains(&minutes) => (minutes, None),
        (Some(_), None) => return Err(Refusal::Invalid("every_minutes")),
        (None, Some(text)) => (0, Some(watch_cron(text)?)),
        _ => return Err(Refusal::EveryOrCron),
    };
    if request
        .for_minutes
        .is_some_and(|minutes| !FOR_MINUTES.contains(&minutes))
    {
        return Err(Refusal::Invalid("for_minutes"));
    }
    let report_to = report_to(&request.report_to)?;
    let run_budget_usd_micros = run_budget(request.run_budget_usd, cfg)?;
    let run_limit_secs = run_limit(request.run_limit_secs, cfg)?;
    skill_path(&folder, &request.skill, &cfg.skill_folders).ok_or(Refusal::UnknownSkill)?;
    Ok(Checked {
        name: request.name.clone(),
        folder,
        skill: request.skill.clone(),
        report_to,
        every_minutes,
        cron,
        for_minutes: request.for_minutes,
        every_run: request.every_run,
        run_budget_usd_micros,
        run_limit_secs,
    })
}

/// A watch's cron expression: it parses, and no two runs are less than 2 minutes apart.
fn watch_cron(text: &str) -> Result<Cron, Refusal> {
    let cron = Cron::parse(text).map_err(Refusal::Cron)?;
    if cron
        .min_gap_minutes()
        .is_some_and(|gap| gap < MIN_CRON_GAP_MINUTES)
    {
        return Err(Refusal::TooOften);
    }
    Ok(cron)
}

/// What a pause or a resume acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    Name(String),
    All,
}

/// Who asked for a pause or a resume. It is for the log only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Caller {
    Cli,
    ControlTower,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckedPause {
    pub target: Target,
    /// `None`: the pause waits for a resume.
    pub for_minutes: Option<u32>,
    pub caller: Caller,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckedResume {
    pub target: Target,
    pub caller: Caller,
}

/// A reminder request that passed every check. Exactly one of `in_minutes` and
/// `due_local` is set. A cron reminder has `due_local`, its next matching minute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckedRemind {
    pub name: String,
    pub folder: PathBuf,
    pub report_to: ReportTo,
    pub in_minutes: Option<u32>,
    /// The next time the local clock reads the asked time: `YYYY-MM-DD HH:MM`.
    pub due_local: Option<String>,
    /// The cron expression of a cron reminder, else `None`.
    pub cron: Option<Cron>,
    pub text: String,
}

fn target(name: &str, all: bool) -> Result<Target, Refusal> {
    match (name.is_empty(), all) {
        (true, true) => Ok(Target::All),
        (false, false) if is_token(name) => Ok(Target::Name(name.to_owned())),
        _ => Err(Refusal::Invalid("name")),
    }
}

fn caller(text: &str) -> Result<Caller, Refusal> {
    match text {
        "" | "cli" => Ok(Caller::Cli),
        "control_tower" => Ok(Caller::ControlTower),
        _ => Err(Refusal::Invalid("caller")),
    }
}

pub(super) fn check_pause(request: &PauseRequest) -> Result<CheckedPause, Refusal> {
    let target = target(&request.name, request.all)?;
    if request
        .for_minutes
        .is_some_and(|minutes| !FOR_MINUTES.contains(&minutes))
    {
        return Err(Refusal::Invalid("for_minutes"));
    }
    Ok(CheckedPause {
        target,
        for_minutes: request.for_minutes,
        caller: caller(&request.caller)?,
    })
}

pub(super) fn check_resume(request: &ResumeRequest) -> Result<CheckedResume, Refusal> {
    Ok(CheckedResume {
        target: target(&request.name, request.all)?,
        caller: caller(&request.caller)?,
    })
}

/// Checks a reminder request. `now` is the local clock, so `at_local` gives the same
/// answer in a test.
pub(super) fn check_remind(
    request: &RemindRequest,
    now: &LocalNow,
) -> Result<CheckedRemind, Refusal> {
    if !is_token(&request.name) {
        return Err(Refusal::Invalid("name"));
    }
    let folder = canonical_folder(&request.folder).ok_or(Refusal::Invalid("folder"))?;
    let (in_minutes, due_local, cron) = match &request.when {
        Some(When::InMinutes(minutes)) if IN_MINUTES.contains(minutes) => {
            (Some(*minutes), None, None)
        }
        Some(When::InMinutes(_)) => return Err(Refusal::Invalid("in_minutes")),
        Some(When::AtLocal(text)) => (None, Some(next_local(text, now)?), None),
        Some(When::Cron(text)) => {
            // One shot: the next matching minute after now. A schedule that parses has a
            // match within 8 years, so `None` cannot come; it is refused as never running.
            let cron = Cron::parse(text).map_err(Refusal::Cron)?;
            let due = cron
                .next_after(now.local_minute)
                .ok_or(Refusal::Cron(CronError::NeverRuns))?;
            (None, Some(due.to_string()), Some(cron))
        }
        None => return Err(Refusal::Invalid("when")),
    };
    let report_to = report_to(&request.report_to)?;
    let text = clean_text(&request.text);
    if text.is_empty() {
        return Err(Refusal::Invalid("text"));
    }
    Ok(CheckedRemind {
        name: request.name.clone(),
        folder,
        report_to,
        in_minutes,
        due_local,
        cron,
        text,
    })
}

/// `HH:MM`, 24 hours, as seconds since midnight.
fn parse_clock(text: &str) -> Option<u32> {
    let (hours, minutes) = text.split_once(':')?;
    let digits = |part: &str| {
        (part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let (hours, minutes) = (digits(hours)?, digits(minutes)?);
    (hours < 24 && minutes < 60).then_some(hours * 3600 + minutes * 60)
}

/// The next time the local clock reads `at`: today when it is still ahead, else tomorrow.
fn next_local(at: &str, now: &LocalNow) -> Result<String, Refusal> {
    const INVALID: Refusal = Refusal::Invalid("at_local");
    let seconds = parse_clock(at).ok_or(INVALID)?;
    let mut parts = now.day.split('-').map(str::parse::<i32>);
    let (Some(Ok(year)), Some(Ok(month)), Some(Ok(day))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(INVALID);
    };
    let month =
        time::Month::try_from(u8::try_from(month).map_err(|_| INVALID)?).map_err(|_| INVALID)?;
    let today =
        time::Date::from_calendar_date(year, month, u8::try_from(day).map_err(|_| INVALID)?)
            .map_err(|_| INVALID)?;
    let date = if seconds > now.seconds_of_day {
        today
    } else {
        today.next_day().ok_or(INVALID)?
    };
    Ok(format!(
        "{:04}-{:02}-{:02} {at}",
        date.year(),
        u8::from(date.month()),
        date.day()
    ))
}

/// A reminder's text without control characters or escape sequences, cut to 300
/// characters. It uses the same cleaning as a watch report. Then it removes the Unicode
/// format characters that change the order or the look of a line (for example a
/// right-to-left override), so the typed line reads as stored.
pub(super) fn clean_text(text: &str) -> String {
    sanitize_bounded_metadata(text, MAX_TEXT_CHARS)
        .chars()
        .filter(|c| !is_format_character(*c))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Zero-width characters, direction marks, embeddings, overrides and isolates, and the
/// byte order mark.
fn is_format_character(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

fn report_to(text: &str) -> Result<ReportTo, Refusal> {
    match text {
        "chief" => Ok(ReportTo::Chief),
        "" | "self" => Ok(ReportTo::SelfPane),
        _ => Err(Refusal::Invalid("report_to")),
    }
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
        let configured = cfg.run_budget_usd.map(to_micros);
        above_daily(configured, cfg)?;
        return Ok(configured);
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
    above_daily(Some(micros), cfg)?;
    Ok(Some(micros))
}

/// Refuses a run budget that is above `[watch] daily_budget_usd`. A run budget equal to
/// the daily budget is accepted.
fn above_daily(budget_micros: Option<i64>, cfg: &WatchSettings) -> Result<(), Refusal> {
    match (budget_micros, cfg.daily_budget_usd) {
        (Some(budget), Some(daily)) if budget > to_micros(daily) => {
            Err(Refusal::RunBudgetAboveDaily(daily))
        }
        _ => Ok(()),
    }
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
