use std::sync::Mutex;

use super::*;

/// The record that the check reads and corrects. `corrects` false makes a correction that
/// changes nothing.
struct FakeRecord {
    held: Mutex<Option<Held>>,
    corrects: bool,
    corrections: Mutex<Vec<(String, u32)>>,
}

impl FakeRecord {
    fn new(pane: &str, pid: u32, corrects: bool) -> Self {
        Self {
            held: Mutex::new(Some(Held {
                pane_id: pane.into(),
                agent_pid: pid,
            })),
            corrects,
            corrections: Mutex::new(Vec::new()),
        }
    }
}

impl RecordPort for FakeRecord {
    fn held(&self, _: &Agent) -> Option<Held> {
        self.held.lock().unwrap().clone()
    }
    fn correct(&self, _: &Agent, pane: &str, pid: u32) {
        self.corrections.lock().unwrap().push((pane.into(), pid));
        if self.corrects {
            *self.held.lock().unwrap() = Some(Held {
                pane_id: pane.into(),
                agent_pid: pid,
            });
        }
    }
}

/// The multiplexer's view: the agent pid of each pane, or a failure.
struct FakeView(Result<Option<u32>, ()>);

impl PaneView for FakeView {
    fn agent_pid(&self, _pane: &str) -> Result<Option<u32>, ViewError> {
        self.0.map_err(|()| ViewError)
    }
}

fn worker() -> Agent {
    Agent::BuildWorker {
        product: "harold".into(),
        spec: "decision-tool-a1".into(),
    }
}

fn input<'a>(
    agent: &'a Agent,
    pane: &'a str,
    pid: u32,
    file: Option<FileClaim>,
) -> IdentityInput<'a> {
    IdentityInput {
        agent,
        hook_pane: pane,
        hook_pid: pid,
        file,
    }
}

#[test]
fn a_record_that_matches_the_hook_and_the_view_is_ok_and_is_not_changed() {
    let record = FakeRecord::new("%41", 4100, true);
    let result = check_identity(
        &input(&worker(), "%41", 4100, None),
        &record,
        &FakeView(Ok(Some(4100))),
    );
    assert_eq!(result.row.outcome, Outcome::Ok);
    assert!(result.fails.is_empty());
    assert!(record.corrections.lock().unwrap().is_empty());
}

#[test]
fn an_old_pane_in_the_record_is_corrected_from_the_view_and_checked_again() {
    let record = FakeRecord::new("%3c", 3100, true);
    let result = check_identity(
        &input(&worker(), "%41", 4100, None),
        &record,
        &FakeView(Ok(Some(4100))),
    );
    assert_eq!(
        *record.corrections.lock().unwrap(),
        vec![("%41".to_string(), 4100)]
    );
    assert_eq!(result.row.outcome, Outcome::Fixed);
    assert!(result.row.detail.contains("%3c") && result.row.detail.contains("%41"));
    assert!(
        result.fails.is_empty(),
        "a corrected mismatch goes to nobody"
    );
}

#[test]
fn a_second_mismatch_after_the_correction_is_a_fail_for_the_receiver() {
    let record = FakeRecord::new("%3c", 3100, false);
    let result = check_identity(
        &input(&worker(), "%41", 4100, None),
        &record,
        &FakeView(Ok(Some(4100))),
    );
    assert_eq!(result.row.outcome, Outcome::Fail);
    assert_eq!(result.fails.len(), 1);
    assert_eq!(result.fails[0].group, Group::Identity);
}

#[test]
fn a_pane_that_holds_another_agent_or_none_is_a_fail_and_the_record_is_not_changed() {
    for view in [Ok(Some(9999)), Ok(None)] {
        let record = FakeRecord::new("%3c", 3100, true);
        let result = check_identity(
            &input(&worker(), "%41", 4100, None),
            &record,
            &FakeView(view),
        );
        assert_eq!(result.row.outcome, Outcome::Fail);
        assert!(record.corrections.lock().unwrap().is_empty());
    }
}

#[test]
fn a_view_that_fails_gives_not_checked_with_the_reason() {
    let record = FakeRecord::new("%41", 4100, true);
    let result = check_identity(
        &input(&worker(), "%41", 4100, None),
        &record,
        &FakeView(Err(())),
    );
    assert_eq!(result.row.outcome, Outcome::NotChecked);
    assert!(result.row.detail.contains("multiplexer_unreachable"));
}

#[test]
fn a_missing_record_is_a_fail() {
    let record = FakeRecord::new("", 0, true);
    *record.held.lock().unwrap() = None;
    let result = check_identity(
        &input(&Agent::Chief, "%7", 700, None),
        &record,
        &FakeView(Ok(Some(700))),
    );
    assert_eq!(result.row.outcome, Outcome::Fail);
    assert!(result.row.detail.contains("no_record"));
}

#[test]
fn a_pane_file_that_names_another_pane_stays_a_fail_for_its_owner_and_the_record_check_goes_on() {
    let record = FakeRecord::new("%41", 4100, true);
    let file = FileClaim {
        name: "agent-pane".into(),
        pane: Some("%3c".into()),
    };
    let result = check_identity(
        &input(&worker(), "%41", 4100, Some(file)),
        &record,
        &FakeView(Ok(Some(4100))),
    );
    assert_eq!(result.row.outcome, Outcome::Fail);
    assert!(result.fails[0].text.contains("agent-pane"));
    assert!(result.fails[0].text.contains("%3c"));
    assert!(record.corrections.lock().unwrap().is_empty());
}

#[test]
fn the_panes_in_an_agent_panes_file_are_pod_coordination_first_then_the_pnem() {
    let text = "%2 pod-coordination\n%5 pnem\n";
    assert_eq!(
        pane_in_file(text, &Agent::PodCoordination("h".into())).as_deref(),
        Some("%2")
    );
    assert_eq!(
        pane_in_file(text, &Agent::Pnem("h".into())).as_deref(),
        Some("%5")
    );
    assert_eq!(pane_in_file("%9\n", &worker()).as_deref(), Some("%9"));
    assert_eq!(pane_in_file("", &Agent::Pnem("h".into())), None);
}

#[test]
fn reading_a_pane_file_never_changes_it() {
    let dir = std::env::temp_dir().join(format!("harold-identity-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent-panes");
    std::fs::write(&path, "%2\n%5\n").unwrap();
    let claim = read_file_claim(&path, &Agent::Pnem("h".into())).unwrap();
    assert_eq!(claim.pane.as_deref(), Some("%5"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "%2\n%5\n");
    assert!(read_file_claim(&dir.join("none"), &Agent::Pnem("h".into())).is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}
