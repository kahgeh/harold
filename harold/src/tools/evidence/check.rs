//! The evidence pass: for each merged spec of a product that has evidence queries on the
//! base branch, it reads the queries from git, runs them through the ClickHouse read, applies
//! the state rule and writes the evidence file. A failed read changes nothing.

use std::path::Path;

use time::{OffsetDateTime, UtcOffset};
use tokio::time::Instant;
use tracing::{info, warn};

use super::files::{EvidenceFile, read_evidence, write_confirmed_once, write_evidence};
use super::state::{QueryResult, Release, State, state_of};
use crate::clickhouse_read::{ClickhouseSource, ReadFailure, fetch_body};
use crate::pod_stats::{Git, POD_FOLDER, Product, SpecKind, key_values, parse_time, spec_kind};
use crate::settings::FactoryFlowSettings;
use crate::telemetry;

/// The folder of the evidence queries in a product's repository.
const QUERY_DIR: &str = "docs/references/telemetry/queries/evidence";
/// A query file over this size is a `bad_query`.
const MAX_QUERY_BYTES: usize = 64 * 1024;
/// The longest `reason`.
const MAX_REASON: usize = 40;

/// What the pass needs: git, the ClickHouse source and the settings.
pub(crate) struct Env<'a, G: Git> {
    pub git: &'a G,
    pub source: &'a dyn ClickhouseSource,
    pub settings: &'a FactoryFlowSettings,
}

/// What one check of one spec gave. With `failed`, nothing was written: `state` and
/// `results` are those of the old file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpecReport {
    pub id: String,
    pub state: State,
    pub results: Vec<QueryResult>,
    /// The `outcome` of the read that failed.
    pub failed: Option<&'static str>,
}

/// A spec with a merged file.
struct Merged {
    at: OffsetDateTime,
    commit: String,
}

/// Checks the specs of `product`: only `only` when it is given (even in a final state), else
/// each spec whose state is not final. A spec with no evidence query gets no report.
pub(crate) async fn check_product<G: Git>(
    env: &Env<'_, G>,
    product: &Product,
    only: Option<&str>,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Vec<SpecReport> {
    let pod = product.repo.join(POD_FOLDER);
    let queries = query_names(env.git, product).await;
    let mut ids: Vec<String> = match only {
        Some(id) => vec![id.to_string()],
        None => merged_ids(&pod),
    };
    ids.sort();
    let mut reports = Vec::new();
    for id in ids {
        let Some(names) = queries.get(&id) else {
            continue;
        };
        let Some(merged) = read_merged(&pod, &id) else {
            continue;
        };
        let old = read_evidence(&pod, &id);
        let was_final = old.as_ref().is_some_and(|file| file.state.is_final());
        if only.is_none() && was_final {
            continue;
        }
        let report = check_spec(env, product, &pod, &id, names, &merged, old, now, offset).await;
        reports.push(report);
    }
    reports
}

#[allow(clippy::too_many_arguments)]
async fn check_spec<G: Git>(
    env: &Env<'_, G>,
    product: &Product,
    pod: &Path,
    id: &str,
    names: &[String],
    merged: &Merged,
    old: Option<EvidenceFile>,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> SpecReport {
    let was_final = old.as_ref().is_some_and(|file| file.state.is_final());
    let mut results = Vec::new();
    for name in names {
        match run_query(env, product, id, name, merged.at).await {
            Ok(result) => results.push(result),
            Err(outcome) => {
                return SpecReport {
                    id: id.to_string(),
                    state: old.as_ref().map_or(State::Waiting, |file| file.state),
                    results: old.map(|file| file.results).unwrap_or_default(),
                    failed: Some(outcome),
                };
            }
        }
    }
    let release = release_of(env.git, product, &merged.commit).await;
    let state = state_of(&results, merged.at, release, now);
    let released = match release {
        Release::Tagged(time) => Some(time.to_offset(offset)),
        _ => None,
    };
    let file = EvidenceFile {
        checked: now.to_offset(offset),
        since: merged.at.to_offset(offset),
        released,
        state,
        results: results.clone(),
        extra_lines: Vec::new(),
    };
    let kind = std::fs::read_to_string(pod.join("specs").join(format!("{id}.md")))
        .map(|text| spec_kind(&text))
        .unwrap_or(SpecKind::Unkinded);
    let mut confirmed = false;
    if write_evidence(pod, id, &file).is_ok() && state == State::Present && kind == SpecKind::Fix {
        let queries: Vec<String> = results.iter().map(|result| result.name.clone()).collect();
        confirmed = write_confirmed_once(pod, id, now.to_offset(offset), &queries).unwrap_or(false);
    }
    info!(
        spec.id = id,
        state = state.as_str(),
        confirmed,
        "evidence file written"
    );
    if state.is_final() && !was_final {
        telemetry::feature_used("evidence-check");
    }
    SpecReport {
        id: id.to_string(),
        state,
        results,
        failed: None,
    }
}

/// Runs the query `name` of spec `id`. `Err` holds the `outcome` that failed.
async fn run_query<G: Git>(
    env: &Env<'_, G>,
    product: &Product,
    id: &str,
    name: &str,
    since: OffsetDateTime,
) -> Result<QueryResult, &'static str> {
    let started = Instant::now();
    let answer = read_query(env, product, name, since).await;
    let outcome = match &answer {
        Ok(_) => "ok",
        Err(outcome) => outcome,
    };
    telemetry::count_evidence_read(outcome);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if outcome == "ok" {
        info!(
            spec.id = id,
            evidence.query = name,
            outcome,
            elapsed_ms,
            "evidence query read"
        );
    } else {
        warn!(
            spec.id = id,
            evidence.query = name,
            outcome,
            elapsed_ms,
            "evidence query read"
        );
    }
    answer.map(|(present, reason)| QueryResult {
        name: name.to_string(),
        present,
        reason,
    })
}

async fn read_query<G: Git>(
    env: &Env<'_, G>,
    product: &Product,
    name: &str,
    since: OffsetDateTime,
) -> Result<(bool, String), &'static str> {
    let path = format!("{QUERY_DIR}/{name}.sql");
    let spec = format!("{}:{path}", product.base);
    let sql = match env.git.run(&product.repo, &["show", &spec]).await {
        Some(output) if output.success && output.stdout.len() <= MAX_QUERY_BYTES => output.stdout,
        _ => return Err("bad_query"),
    };
    let since_text = clickhouse_time(since);
    let body = fetch_body(
        env.settings.users_file.as_deref(),
        &env.settings.clickhouse_url,
        &sql,
        &[("since", &since_text)],
        env.source,
    )
    .await
    .map_err(|failure| match failure {
        ReadFailure::NotSetUp => "not_set_up",
        ReadFailure::Unreachable => "unreachable",
        ReadFailure::Refused => "refused",
        ReadFailure::Timeout => "timeout",
    })?;
    row_in(&body).ok_or("bad_row")
}

/// `YYYY-MM-DD HH:MM:SS` in UTC, the text of a ClickHouse `DateTime` parameter.
fn clickhouse_time(time: OffsetDateTime) -> String {
    let time = time.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

/// The one row of `body`: `present` as 0 or 1 and a `reason` of 1 to 40 characters from `a`
/// to `z` and `_`. None for any other body.
fn row_in(body: &str) -> Option<(bool, String)> {
    let mut lines = body.lines().filter(|line| !line.trim().is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let present = match value.get("present")? {
        serde_json::Value::Number(number) => number.as_u64()?,
        serde_json::Value::String(text) => text.parse().ok()?,
        _ => return None,
    };
    let reason = value.get("reason")?.as_str()?;
    let word = !reason.is_empty()
        && reason.len() <= MAX_REASON
        && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    (present <= 1 && word).then(|| (present == 1, reason.to_string()))
}

/// The evidence query names on the base branch, by spec id, in the order of `<n>`.
async fn query_names<G: Git>(
    git: &G,
    product: &Product,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut by_id: std::collections::HashMap<String, Vec<(u32, String)>> = Default::default();
    let folder = format!("{QUERY_DIR}/");
    let Some(listing) = git
        .run(
            &product.repo,
            &["ls-tree", "--name-only", &product.base, &folder],
        )
        .await
        .filter(|output| output.success)
    else {
        return Default::default();
    };
    for line in listing.stdout.lines() {
        let Some(stem) = line
            .strip_prefix(&folder)
            .and_then(|name| name.strip_suffix(".sql"))
        else {
            continue;
        };
        let Some((id, number)) = stem.rsplit_once('-') else {
            continue;
        };
        let Ok(number) = number.parse::<u32>() else {
            continue;
        };
        by_id
            .entry(id.to_string())
            .or_default()
            .push((number, stem.to_string()));
    }
    by_id
        .into_iter()
        .map(|(id, mut names)| {
            names.sort();
            (id, names.into_iter().map(|(_, name)| name).collect())
        })
        .collect()
}

/// The ids of the files in `merged/`.
fn merged_ids(pod: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(pod.join("merged")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md").map(str::to_string)
        })
        .collect()
}

fn read_merged(pod: &Path, id: &str) -> Option<Merged> {
    let text = std::fs::read_to_string(pod.join("merged").join(format!("{id}.md"))).ok()?;
    let pairs = key_values(&text);
    let at = parse_time(pairs.get("merged")?)?;
    let commit = (*pairs.get("commit")?).to_string();
    let is_sha = commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit());
    is_sha.then_some(Merged { at, commit })
}

/// The release of `commit`: the time of the first `v*` tag that holds it.
async fn release_of<G: Git>(git: &G, product: &Product, commit: &str) -> Release {
    let any = git
        .run(&product.repo, &["tag", "--list", "v[0-9]*"])
        .await
        .filter(|output| output.success);
    if any.is_some_and(|output| output.stdout.trim().is_empty()) {
        return Release::NoTags;
    }
    let holding = git
        .run(
            &product.repo,
            &[
                "for-each-ref",
                "--contains",
                commit,
                "--sort=creatordate",
                "--format=%(creatordate:unix)",
                "refs/tags/v[0-9]*",
            ],
        )
        .await
        .filter(|output| output.success);
    holding
        .and_then(|output| output.stdout.lines().next().map(str::to_string))
        .and_then(|line| line.trim().parse::<i64>().ok())
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .map_or(Release::NotYet, Release::Tagged)
}
