//! Tests of the evidence file and the confirmed file, in a temporary pod folder.

use std::path::{Path, PathBuf};

use super::files::{EvidenceFile, read_evidence, write_confirmed_once, write_evidence};
use super::state::{QueryResult, State};

fn t(text: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
}

struct Pod(PathBuf);

impl Pod {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("harold-evidence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create the pod folder");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file(state: State) -> EvidenceFile {
    EvidenceFile {
        checked: t("2026-11-20T09:00:00+11:00"),
        since: t("2026-11-12T14:05:00+11:00"),
        released: Some(t("2026-11-13T10:00:00+11:00")),
        state,
        results: vec![
            QueryResult {
                name: "evidence-check-1".to_string(),
                present: true,
                reason: "gauge_seen".to_string(),
            },
            QueryResult {
                name: "evidence-check-2".to_string(),
                present: false,
                reason: "no_ok_read".to_string(),
            },
        ],
        extra_lines: Vec::new(),
    }
}

#[test]
fn the_file_has_the_lines_of_the_spec_in_order() {
    let pod = Pod::new();
    write_evidence(pod.path(), "evidence-check", &file(State::Waiting)).unwrap();
    let text = std::fs::read_to_string(pod.path().join("evidence/evidence-check.md")).unwrap();
    assert_eq!(
        text,
        "# Evidence: evidence-check\n\
         \n\
         checked: 2026-11-20T09:00:00+11:00\n\
         since: 2026-11-12T14:05:00+11:00\n\
         released: 2026-11-13T10:00:00+11:00\n\
         state: waiting\n\
         query: evidence-check-1 present gauge_seen\n\
         query: evidence-check-2 absent no_ok_read\n"
    );
}

#[test]
fn released_is_empty_while_no_tag_holds_the_merge() {
    let pod = Pod::new();
    let mut open = file(State::Waiting);
    open.released = None;
    write_evidence(pod.path(), "a", &open).unwrap();
    let text = std::fs::read_to_string(pod.path().join("evidence/a.md")).unwrap();
    assert!(text.contains("\nreleased:\n"), "{text}");
    let back = read_evidence(pod.path(), "a").unwrap();
    assert_eq!(back.released, None);
}

#[test]
fn a_file_reads_back_as_written() {
    let pod = Pod::new();
    write_evidence(pod.path(), "a", &file(State::Missing)).unwrap();
    let back = read_evidence(pod.path(), "a").unwrap();
    assert_eq!(back.state, State::Missing);
    assert_eq!(back.results, file(State::Missing).results);
    assert_eq!(back.since, t("2026-11-12T14:05:00+11:00"));
}

#[test]
fn a_rewrite_keeps_the_told_line() {
    let pod = Pod::new();
    write_evidence(pod.path(), "a", &file(State::Missing)).unwrap();
    let path = pod.path().join("evidence/a.md");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("told: 2026-11-21T08:00:00+11:00\n");
    std::fs::write(&path, text).unwrap();
    write_evidence(pod.path(), "a", &file(State::Missing)).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after.matches("told: 2026-11-21T08:00:00+11:00\n").count(),
        1,
        "{after}"
    );
    write_evidence(pod.path(), "a", &file(State::Present)).unwrap();
    let again = std::fs::read_to_string(&path).unwrap();
    assert_eq!(again.matches("told:").count(), 1, "{again}");
}

#[test]
fn a_write_leaves_no_temporary_file() {
    let pod = Pod::new();
    write_evidence(pod.path(), "a", &file(State::Waiting)).unwrap();
    let names: Vec<String> = std::fs::read_dir(pod.path().join("evidence"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.md"]);
}

#[test]
fn a_file_that_does_not_parse_reads_as_none() {
    let pod = Pod::new();
    std::fs::create_dir_all(pod.path().join("evidence")).unwrap();
    std::fs::write(pod.path().join("evidence/a.md"), "state: maybe\n").unwrap();
    assert!(read_evidence(pod.path(), "a").is_none());
}

#[test]
fn the_confirmed_file_is_written_once() {
    let pod = Pod::new();
    let at = t("2026-11-20T09:00:00+11:00");
    let names = ["q-1".to_string(), "q-2".to_string()];
    assert!(write_confirmed_once(pod.path(), "a", at, &names).unwrap());
    let path = pod.path().join("confirmed/a.md");
    let first = std::fs::read_to_string(&path).unwrap();
    assert!(
        first.contains("confirmed: 2026-11-20T09:00:00+11:00\n"),
        "{first}"
    );
    assert!(first.contains("evidence: q-1 q-2\n"), "{first}");
    let later = t("2026-11-22T09:00:00+11:00");
    assert!(!write_confirmed_once(pod.path(), "a", later, &names).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
}
