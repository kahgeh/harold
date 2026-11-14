//! The `harold tool decision` command line: what it parses, and the text it prints. The
//! calls go through `cli.rs`, as for the other tools.

use harold_api::tools::decision::v1 as pb;

pub(crate) const USAGE_LINE: &str = "invalid argument: usage is harold tool decision add --role R --kind K --details-owner O --accountable A --title T --check DATE --details PLACE:PATH#ID [--link L] [--asked-at TIME] | list [--due] [--open] [--all] [--role R] [--kind K] [--details-owner O] [--accountable A] [--json] | show ID | evaluate ID --process good|poor --outcome good|poor | recheck ID --check DATE | link ID LINK";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ListFlags {
    pub due: bool,
    pub open: bool,
    pub all: bool,
    pub role: String,
    pub kind: String,
    pub details_owner: String,
    pub accountable: String,
    pub json: bool,
}

/// A `harold tool decision` command. The daemon checks the values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecisionCommand {
    Add {
        role: String,
        kind: String,
        details_owner: String,
        accountable: String,
        title: String,
        /// Empty when not given.
        link: String,
        check: String,
        details: String,
        /// Empty when not given.
        asked_at: String,
    },
    List(ListFlags),
    Show {
        id: String,
    },
    Evaluate {
        id: String,
        process: String,
        outcome: String,
    },
    Recheck {
        id: String,
        check: String,
    },
    Link {
        id: String,
        link: String,
    },
}

/// Reads the flags `valued` (each takes a word) and `plain` (each takes none) from `args`.
/// A flag that repeats, an unknown flag and a flag with no word are refused.
fn flags(
    args: &[&str],
    valued: &[&str],
    plain: &[&str],
) -> Result<(std::collections::HashMap<String, String>, Vec<String>), String> {
    let mut found = std::collections::HashMap::new();
    let mut switches = Vec::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        if plain.contains(flag) {
            if switches.iter().any(|seen| seen == flag) {
                return Err(USAGE_LINE.into());
            }
            switches.push((*flag).to_owned());
        } else if valued.contains(flag) {
            let value = rest.next().ok_or_else(|| USAGE_LINE.to_string())?;
            if found
                .insert((*flag).to_owned(), (*value).to_owned())
                .is_some()
            {
                return Err(USAGE_LINE.into());
            }
        } else {
            return Err(USAGE_LINE.into());
        }
    }
    Ok((found, switches))
}

pub(crate) fn parse(args: &[&str]) -> Result<DecisionCommand, String> {
    let usage = || USAGE_LINE.to_string();
    match args {
        ["add", rest @ ..] => {
            let (mut found, _) = flags(
                rest,
                &[
                    "--role",
                    "--kind",
                    "--details-owner",
                    "--accountable",
                    "--title",
                    "--link",
                    "--check",
                    "--details",
                    "--asked-at",
                ],
                &[],
            )?;
            let mut take = |flag: &str| found.remove(flag);
            let mut need = |flag: &str| take(flag).ok_or_else(usage);
            Ok(DecisionCommand::Add {
                role: need("--role")?,
                kind: need("--kind")?,
                details_owner: need("--details-owner")?,
                accountable: need("--accountable")?,
                title: need("--title")?,
                check: need("--check")?,
                details: need("--details")?,
                link: found.remove("--link").unwrap_or_default(),
                asked_at: found.remove("--asked-at").unwrap_or_default(),
            })
        }
        ["list", rest @ ..] => {
            let (mut found, switches) = flags(
                rest,
                &["--role", "--kind", "--details-owner", "--accountable"],
                &["--due", "--open", "--all", "--json"],
            )?;
            let on = |name: &str| switches.iter().any(|seen| seen == name);
            Ok(DecisionCommand::List(ListFlags {
                due: on("--due"),
                open: on("--open"),
                all: on("--all"),
                json: on("--json"),
                role: found.remove("--role").unwrap_or_default(),
                kind: found.remove("--kind").unwrap_or_default(),
                details_owner: found.remove("--details-owner").unwrap_or_default(),
                accountable: found.remove("--accountable").unwrap_or_default(),
            }))
        }
        ["show", id] if !id.starts_with('-') => Ok(DecisionCommand::Show { id: (*id).into() }),
        ["evaluate", id, rest @ ..] if !id.starts_with('-') => {
            let (mut found, _) = flags(rest, &["--process", "--outcome"], &[])?;
            Ok(DecisionCommand::Evaluate {
                id: (*id).into(),
                process: found.remove("--process").ok_or_else(usage)?,
                outcome: found.remove("--outcome").ok_or_else(usage)?,
            })
        }
        ["recheck", id, rest @ ..] if !id.starts_with('-') => {
            let (mut found, _) = flags(rest, &["--check"], &[])?;
            Ok(DecisionCommand::Recheck {
                id: (*id).into(),
                check: found.remove("--check").ok_or_else(usage)?,
            })
        }
        ["link", id, link] if !id.starts_with('-') => Ok(DecisionCommand::Link {
            id: (*id).into(),
            link: (*link).into(),
        }),
        _ => Err(usage()),
    }
}

pub(crate) fn added_line(row: &pb::Decision) -> String {
    format!("added {} (check {})", row.id, row.check_on)
}

pub(crate) fn evaluated_line(row: &pb::Decision) -> String {
    format!(
        "evaluated {} (process {}, outcome {})",
        row.id, row.process_grade, row.outcome_grade
    )
}

pub(crate) fn rechecked_line(row: &pb::Decision) -> String {
    format!(
        "rechecked {} (check {}, {})",
        row.id, row.check_on, row.status
    )
}

pub(crate) fn linked_line(row: &pb::Decision) -> String {
    format!("linked {} to {}", row.id, row.link)
}

/// One record with every field, and the full path of its details entry.
pub(crate) fn show_text(row: &pb::Decision, details_path: &str) -> String {
    let mut lines = vec![
        ("id", row.id.as_str()),
        ("decided on", &row.decided_on),
        ("organisation", &row.organisation),
        ("role", &row.role),
        ("kind", &row.kind),
        ("details owner", &row.details_owner),
        ("accountable", &row.accountable),
        ("title", &row.title),
        ("link", &row.link),
        ("check", &row.check_on),
        ("status", &row.status),
        ("process grade", &row.process_grade),
        ("outcome grade", &row.outcome_grade),
        ("details", &row.details),
        ("details path", details_path),
    ]
    .into_iter()
    .map(|(name, value)| format!("{name}: {value}"))
    .collect::<Vec<_>>();
    for (name, ms) in [
        ("asked at ms", row.asked_at_ms),
        ("added at ms", row.added_at_ms),
        ("evaluated at ms", row.evaluated_at_ms),
    ] {
        lines.push(format!("{name}: {ms}"));
    }
    lines.join("\n")
}

fn json_of(row: &pb::Decision) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "decided_on": row.decided_on,
        "organisation": row.organisation,
        "role": row.role,
        "kind": row.kind,
        "details_owner": row.details_owner,
        "accountable": row.accountable,
        "title": row.title,
        "link": row.link,
        "check_on": row.check_on,
        "status": row.status,
        "process_grade": row.process_grade,
        "outcome_grade": row.outcome_grade,
        "details": row.details,
        "asked_at_ms": row.asked_at_ms,
        "added_at_ms": row.added_at_ms,
        "evaluated_at_ms": row.evaluated_at_ms,
    })
}

/// The list: one line for each record, with the columns padded, and a count line. With
/// `--json`, an array that holds every field.
pub(crate) fn list_text(rows: &[pb::Decision], flags: &ListFlags) -> String {
    if flags.json {
        let all: Vec<_> = rows.iter().map(json_of).collect();
        return serde_json::to_string(&all).unwrap_or_else(|_| "[]".into());
    }
    let width = |pick: fn(&pb::Decision) -> &str| {
        rows.iter()
            .map(|row| pick(row).chars().count())
            .max()
            .unwrap_or(0)
    };
    let (role, kind, who, title) = (
        width(|r| &r.role),
        width(|r| &r.kind),
        width(|r| &r.accountable),
        width(|r| &r.title),
    );
    let mut lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{}  {}  {:role$}  {:kind$}  {:who$}  {:title$}  check {}  {}",
                r.id, r.decided_on, r.role, r.kind, r.accountable, r.title, r.check_on, r.status
            )
        })
        .collect();
    let what = match (flags.all, flags.due, flags.open) {
        (true, _, _) => "records",
        (_, true, false) => "due",
        (_, false, true) => "open",
        _ => "open or due",
    };
    lines.push(format!("{} {what}", rows.len()));
    lines.join("\n")
}

/// The one line printed for a refusal from the decision service.
pub(crate) fn refusal_line(status: &tonic::Status) -> String {
    match (status.code(), status.message()) {
        (tonic::Code::Unimplemented, _) => {
            "the Harold daemon does not serve decision records".into()
        }
        (tonic::Code::PermissionDenied, "not_chief") => {
            "only the chief's pane can run this command".into()
        }
        (tonic::Code::PermissionDenied, "no_pane") => {
            "run this from a pane: no TMUX_PANE or REX_BLOCK".into()
        }
        (tonic::Code::NotFound, "unknown_decision") => "unknown decision record".into(),
        (tonic::Code::FailedPrecondition, "already_evaluated") => {
            "this decision record is evaluated already".into()
        }
        _ if status.message().is_empty() => "the Harold daemon refused the call".into(),
        _ => status.message().to_string(),
    }
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
