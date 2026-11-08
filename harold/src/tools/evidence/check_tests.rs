//! Tests of the evidence pass, on temporary git repositories, a fake source, a temporary pod
//! folder and a fake clock. No test reads a real repository, the real pod folder or the real
//! ClickHouse.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Mutex;

use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use super::check::{Env, check_product};
use super::files::read_evidence;
use super::state::State;
use crate::clickhouse_read::{ClickhouseRequest, ClickhouseSource, SourceFailure};
use crate::pod_stats::{Product, SystemGit};
use crate::settings::FactoryFlowSettings;
use crate::telemetry::test_support;

fn t(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

/// Every pass counts the one feature `evidence-check`, so the tests run one at a time.
pub(super) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const QUERY_DIR: &str = "docs/references/telemetry/queries/evidence";

/// The `(sql, params)` of one request.
pub(super) type Seen = (String, Vec<(String, String)>);

/// One answer of the fake source for each call, in order. The last answer repeats.
pub(super) struct FakeSource {
    pub(super) answers: Mutex<Vec<Result<String, SourceFailure>>>,
    /// The `(sql, params)` of each request.
    pub(super) seen: Mutex<Vec<Seen>>,
}

impl FakeSource {
    pub(super) fn new(answers: Vec<Result<String, SourceFailure>>) -> Self {
        Self {
            answers: Mutex::new(answers),
            seen: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn row(present: &str, reason: &str) -> Result<String, SourceFailure> {
        Ok(format!(
            "{{\"present\":{present},\"reason\":\"{reason}\"}}\n"
        ))
    }
}

impl ClickhouseSource for FakeSource {
    fn query<'a>(
        &'a self,
        request: &'a ClickhouseRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>> {
        self.seen
            .lock()
            .unwrap()
            .push((request.sql().to_string(), request.params().to_vec()));
        let mut answers = self.answers.lock().unwrap();
        let answer = if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        };
        Box::pin(async move { answer })
    }
}

pub(super) struct Fixture {
    pub(super) root: PathBuf,
    pub(super) repo: PathBuf,
    pub(super) users: PathBuf,
    next_id: u32,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("harold-evcheck-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let repo = root.join("repo");
        git(&repo, &[], &["init", "-q", "-b", "main"]);
        let users = root.join("users.xml");
        std::fs::write(&users, "CREATE USER hyperdx_ro IDENTIFIED BY 'fake-pw';\n").unwrap();
        Self {
            root,
            repo,
            users,
            next_id: 0,
        }
    }

    pub(super) fn pod(&self) -> PathBuf {
        self.repo.join(".superpowers/pod")
    }

    pub(super) fn product(&self) -> Product {
        Product {
            name: "evidence-fixture".to_string(),
            repo: self.repo.clone(),
            base: "main".to_string(),
            slots: 3,
            paused: false,
        }
    }

    pub(super) fn settings(&self) -> FactoryFlowSettings {
        FactoryFlowSettings {
            users_file: Some(self.users.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    /// Commits the query files `files` (name, text) on `main` at `when`; returns the commit.
    fn commit(&mut self, files: &[(&str, &str)], when: &str) -> String {
        self.next_id += 1;
        for (name, text) in files {
            write(&self.repo.join(QUERY_DIR).join(name), text);
        }
        write(&self.repo.join(format!("n{}", self.next_id)), "x");
        git(
            &self.repo,
            &[("GIT_COMMITTER_DATE", when), ("GIT_AUTHOR_DATE", when)],
            &["add", "-A"],
        );
        git(
            &self.repo,
            &[("GIT_COMMITTER_DATE", when), ("GIT_AUTHOR_DATE", when)],
            &["commit", "-q", "-m", "c"],
        );
        git(&self.repo, &[], &["rev-parse", "HEAD"])
    }

    pub(super) fn tag(&self, name: &str) {
        git(&self.repo, &[], &["tag", name]);
    }

    /// `specs/<id>.md` with the kind, and `merged/<id>.md` with the commit.
    fn merged(&self, id: &str, kind: &str, commit: &str, at: &str) {
        write(
            &self.pod().join("specs").join(format!("{id}.md")),
            &format!("# {id}\n\n**Kind.** {kind}\n"),
        );
        write(
            &self.pod().join("merged").join(format!("{id}.md")),
            &format!("# Merged: {id}\n\nmerged: {at}\ncommit: {commit}\n"),
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn git(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .envs(env.iter().copied())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(super) const MERGE_TIME: &str = "2026-11-12T03:05:00Z";
const TAG_TIME: &str = "2026-11-13T10:00:00Z";
pub(super) const SQL: &str = "SELECT 1 AS present, 'x' AS reason WHERE now() > {since:DateTime}";

async fn run(
    fixture: &Fixture,
    source: &FakeSource,
    only: Option<&str>,
    now: &str,
) -> Vec<super::check::SpecReport> {
    let git = SystemGit::default();
    let settings = fixture.settings();
    let env = Env {
        git: &git,
        source,
        settings: &settings,
    };
    check_product(&env, &fixture.product(), only, t(now), UtcOffset::UTC).await
}

/// A fixture with one spec `s` merged at MERGE_TIME and a tag at TAG_TIME that holds it.
pub(super) fn tagged(queries: &[(&str, &str)]) -> Fixture {
    let mut fixture = Fixture::new();
    let commit = fixture.commit(queries, TAG_TIME);
    fixture.tag("v1.0.0");
    fixture.merged("s", "feature", &commit, MERGE_TIME);
    fixture
}

#[tokio::test]
async fn all_present_writes_present_and_the_since_goes_as_a_parameter() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    let reports = run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].state, State::Present);
    let file = read_evidence(&fixture.pod(), "s").expect("file");
    assert_eq!(file.state, State::Present);
    assert_eq!(file.released, Some(t(TAG_TIME)));
    assert_eq!(file.since, t(MERGE_TIME));
    let seen = source.seen.lock().unwrap();
    assert_eq!(seen[0].0, SQL);
    assert!(
        seen[0]
            .1
            .contains(&("param_since".to_string(), "2026-11-12 03:05:00".to_string()))
    );
    assert!(!seen[0].0.contains("2026-11-12"));
}

#[tokio::test]
async fn one_absent_waits_before_seven_days_and_is_missing_after() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]);
    let early = run(&fixture, &source, None, "2026-11-20T09:59:00Z").await;
    assert_eq!(early[0].state, State::Waiting);
    let late = run(&fixture, &source, None, "2026-11-20T10:00:00Z").await;
    assert_eq!(late[0].state, State::Missing);
}

#[tokio::test]
async fn a_spec_with_no_tag_on_its_merge_waits() {
    let _serial = SERIAL.lock().await;
    let mut fixture = Fixture::new();
    let first = fixture.commit(&[], TAG_TIME);
    fixture.tag("v1.0.0");
    let second = fixture.commit(&[("s-1.sql", SQL)], "2026-11-14T00:00:00Z");
    fixture.merged("s", "feature", &second, MERGE_TIME);
    assert_ne!(first, second);
    let source = FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]);
    let reports = run(&fixture, &source, None, "2027-03-01T00:00:00Z").await;
    assert_eq!(reports[0].state, State::Waiting);
    assert_eq!(read_evidence(&fixture.pod(), "s").unwrap().released, None);
}

#[tokio::test]
async fn a_product_with_no_tag_counts_from_the_merge() {
    let _serial = SERIAL.lock().await;
    let mut fixture = Fixture::new();
    let commit = fixture.commit(&[("s-1.sql", SQL)], MERGE_TIME);
    fixture.merged("s", "feature", &commit, MERGE_TIME);
    let source = FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]);
    let reports = run(&fixture, &source, None, "2026-11-19T03:05:00Z").await;
    assert_eq!(reports[0].state, State::Missing);
}

#[tokio::test]
async fn the_query_text_comes_from_the_base_branch_not_the_working_tree() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    write(
        &fixture.repo.join(QUERY_DIR).join("s-1.sql"),
        "SELECT 'edited'",
    );
    write(
        &fixture.repo.join(QUERY_DIR).join("s-2.sql"),
        "SELECT 'unmerged'",
    );
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    let seen = source.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, SQL);
}

#[tokio::test]
async fn a_merged_spec_with_no_evidence_query_gets_no_file() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("other-1.sql", SQL)]);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    let reports = run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    assert!(reports.is_empty());
    assert!(!fixture.pod().join("evidence/s.md").exists());
}

#[tokio::test]
async fn a_failed_read_keeps_the_old_file_and_counts_its_outcome() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let good = FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]);
    run(&fixture, &good, None, "2026-11-14T00:00:00Z").await;
    let before = std::fs::read_to_string(fixture.pod().join("evidence/s.md")).unwrap();
    for (failure, outcome) in [
        (SourceFailure::Refused, "refused"),
        (SourceFailure::Unreachable, "unreachable"),
        (SourceFailure::Timeout, "timeout"),
    ] {
        let counted = || {
            test_support::exact_counter_value("harold.evidence.reads", &[("outcome", outcome)])
                .unwrap_or(0)
        };
        let was = counted();
        let bad = FakeSource::new(vec![Err(failure)]);
        run(&fixture, &bad, None, "2026-11-25T00:00:00Z").await;
        assert!(counted() > was, "{outcome}");
        let after = std::fs::read_to_string(fixture.pod().join("evidence/s.md")).unwrap();
        assert_eq!(after, before, "{outcome}");
    }
}

#[tokio::test]
async fn no_users_file_counts_not_set_up() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let git = SystemGit::default();
    let settings = FactoryFlowSettings::default();
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    let env = Env {
        git: &git,
        source: &source,
        settings: &settings,
    };
    let was =
        test_support::exact_counter_value("harold.evidence.reads", &[("outcome", "not_set_up")])
            .unwrap_or(0);
    check_product(
        &env,
        &fixture.product(),
        None,
        t("2026-11-14T00:00:00Z"),
        UtcOffset::UTC,
    )
    .await;
    let now =
        test_support::exact_counter_value("harold.evidence.reads", &[("outcome", "not_set_up")])
            .unwrap();
    assert!(now > was);
    assert!(source.seen.lock().unwrap().is_empty());
    assert!(!fixture.pod().join("evidence/s.md").exists());
}

#[tokio::test]
async fn a_bad_row_is_counted_and_keeps_the_old_file() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let bodies = [
        "{\"present\":2,\"reason\":\"use_seen\"}\n",
        "{\"present\":1,\"reason\":\"Use Seen\"}\n",
        "{\"present\":1,\"reason\":\"\"}\n",
        "{\"present\":1,\"reason\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n",
        "",
        "{\"present\":1,\"reason\":\"a\"}\n{\"present\":1,\"reason\":\"b\"}\n",
        "not json\n",
    ];
    for body in bodies {
        let fixture = tagged(&[("s-1.sql", SQL)]);
        let was =
            test_support::exact_counter_value("harold.evidence.reads", &[("outcome", "bad_row")])
                .unwrap_or(0);
        let source = FakeSource::new(vec![Ok(body.to_string())]);
        let reports = run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
        assert_eq!(reports[0].state, State::Waiting, "{body:?}");
        assert!(!fixture.pod().join("evidence/s.md").exists(), "{body:?}");
        let now =
            test_support::exact_counter_value("harold.evidence.reads", &[("outcome", "bad_row")])
                .unwrap();
        assert!(now > was, "{body:?}");
    }
}

#[tokio::test]
async fn a_reason_of_forty_characters_and_a_string_one_are_good_rows() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let reason = "a".repeat(40);
    let body = format!("{{\"present\":\"1\",\"reason\":\"{reason}\"}}\n");
    let source = FakeSource::new(vec![Ok(body)]);
    let reports = run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    assert_eq!(reports[0].state, State::Present);
}

#[tokio::test]
async fn a_file_over_64_kib_is_a_bad_query_and_exactly_64_kib_is_read() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let padding = |size: usize| format!("SELECT 1 /*{}*/", "x".repeat(size - 13));
    let big = padding(64 * 1024 + 1);
    let fixture = tagged(&[("s-1.sql", &big)]);
    let was =
        test_support::exact_counter_value("harold.evidence.reads", &[("outcome", "bad_query")])
            .unwrap_or(0);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    let now =
        test_support::exact_counter_value("harold.evidence.reads", &[("outcome", "bad_query")])
            .unwrap();
    assert!(now > was);
    assert!(source.seen.lock().unwrap().is_empty());
    assert!(!fixture.pod().join("evidence/s.md").exists());

    assert_eq!(padding(64 * 1024).len(), 64 * 1024);
    let exact = padding(64 * 1024);
    let fixture = tagged(&[("s-1.sql", &exact)]);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    let reports = run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    assert_eq!(reports[0].state, State::Present);
}

#[tokio::test]
async fn a_fix_gets_the_confirmed_file_once_and_a_feature_gets_none() {
    let _serial = SERIAL.lock().await;
    let mut fixture = Fixture::new();
    let commit = fixture.commit(&[("fx-1.sql", SQL), ("ft-1.sql", SQL)], TAG_TIME);
    fixture.tag("v1.0.0");
    fixture.merged("fx", "fix", &commit, MERGE_TIME);
    fixture.merged("ft", "feature", &commit, MERGE_TIME);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    let confirmed = fixture.pod().join("confirmed/fx.md");
    let first = std::fs::read_to_string(&confirmed).expect("confirmed file");
    assert!(first.contains("confirmed: 2026-11-14T00:00:00Z"), "{first}");
    assert!(first.contains("evidence: fx-1"), "{first}");
    assert!(!fixture.pod().join("confirmed/ft.md").exists());
    run(&fixture, &source, Some("fx"), "2026-11-15T00:00:00Z").await;
    assert_eq!(std::fs::read_to_string(&confirmed).unwrap(), first);
}

#[tokio::test]
async fn the_pass_skips_a_final_state_and_the_command_with_an_id_checks_it_again() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    assert_eq!(source.seen.lock().unwrap().len(), 1);
    let again = run(&fixture, &source, None, "2026-11-15T00:00:00Z").await;
    assert!(again.is_empty());
    assert_eq!(source.seen.lock().unwrap().len(), 1);
    let named = run(&fixture, &source, Some("s"), "2026-11-16T00:00:00Z").await;
    assert_eq!(named.len(), 1);
    assert_eq!(source.seen.lock().unwrap().len(), 2);
    assert_eq!(
        read_evidence(&fixture.pod(), "s").unwrap().checked,
        t("2026-11-16T00:00:00Z")
    );
}

#[tokio::test]
async fn the_daily_check_and_the_command_both_keep_the_told_line() {
    let _serial = SERIAL.lock().await;
    let fixture = tagged(&[("s-1.sql", SQL)]);
    let source = FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]);
    run(&fixture, &source, None, "2026-11-14T00:00:00Z").await;
    let path = fixture.pod().join("evidence/s.md");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("told: 2026-11-15T00:00:00Z\n");
    std::fs::write(&path, text).unwrap();
    run(&fixture, &source, None, "2026-11-16T00:00:00Z").await;
    run(&fixture, &source, Some("s"), "2026-11-17T00:00:00Z").await;
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after.matches("told: 2026-11-15T00:00:00Z").count(),
        1,
        "{after}"
    );
}

#[tokio::test]
async fn a_final_state_counts_the_feature_once() {
    let _serial = SERIAL.lock().await;
    test_support::install();
    let feature = || {
        test_support::exact_counter_value("harold.feature.used", &[("spec.id", "evidence-check")])
            .unwrap_or(0)
    };
    let mut fixture = Fixture::new();
    let commit = fixture.commit(&[("evidence-check-1.sql", SQL)], TAG_TIME);
    fixture.tag("v1.0.0");
    fixture.merged("evidence-check", "feature", &commit, MERGE_TIME);
    let source = FakeSource::new(vec![FakeSource::row("1", "use_seen")]);
    let waiting = FakeSource::new(vec![FakeSource::row("0", "no_use_yet")]);
    let start = feature();
    run(&fixture, &waiting, None, "2026-11-14T00:00:00Z").await;
    assert_eq!(feature(), start, "waiting is not final");
    run(&fixture, &source, None, "2026-11-15T00:00:00Z").await;
    assert_eq!(feature(), start + 1);
    run(
        &fixture,
        &source,
        Some("evidence-check"),
        "2026-11-16T00:00:00Z",
    )
    .await;
    assert_eq!(feature(), start + 1, "a final state counts once");
}
