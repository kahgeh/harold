use std::collections::HashMap;
use std::sync::Mutex;

use super::*;

fn pnem() -> Agent {
    Agent::Pnem("harold".into())
}
fn coordination() -> Agent {
    Agent::PodCoordination("harold".into())
}
fn worker() -> Agent {
    Agent::BuildWorker {
        product: "harold".into(),
        spec: "decision-tool-a1".into(),
    }
}

/// Every port answer is set by the test.
#[derive(Default)]
struct Fake {
    /// What the sender's lookup gives for a receiver title: pane and pid, or a reason.
    lookups: HashMap<String, Result<Resolved, &'static str>>,
    /// The agent pid that runs now, for each receiver title.
    running: HashMap<String, u32>,
    sockets: HashMap<u32, SocketProbe>,
    panes: HashMap<String, Result<Option<u32>, ()>>,
    probed: Mutex<Vec<u32>>,
}

impl LinkPorts for Fake {
    fn lookup(&self, _sender: &Agent, receiver: &Agent) -> Result<Resolved, &'static str> {
        self.lookups
            .get(&receiver.title())
            .cloned()
            .unwrap_or(Err("no_record"))
    }
    fn running(&self, receiver: &Agent) -> Option<u32> {
        self.running.get(&receiver.title()).copied()
    }
    fn socket(&self, pid: u32) -> SocketProbe {
        self.probed.lock().unwrap().push(pid);
        self.sockets
            .get(&pid)
            .copied()
            .unwrap_or(SocketProbe::Unknown)
    }
    fn pane_agent_pid(&self, pane: &str) -> Result<Option<u32>, ViewError> {
        self.panes
            .get(pane)
            .cloned()
            .unwrap_or(Ok(None))
            .map_err(|()| ViewError)
    }
}

fn resolved(pane: &str, pid: u32) -> Result<Resolved, &'static str> {
    Ok(Resolved {
        pane_id: pane.into(),
        agent_pid: pid,
    })
}

fn healthy() -> Fake {
    let mut fake = Fake::default();
    for (title, pane, pid) in [
        ("chief", "%1", 100),
        ("pnem harold", "%2", 200),
        ("pod-coordination harold", "%3", 300),
    ] {
        fake.lookups.insert(title.into(), resolved(pane, pid));
        fake.running.insert(title.into(), pid);
        fake.sockets.insert(pid, SocketProbe::Accepts);
        fake.panes.insert(pane.into(), Ok(Some(pid)));
    }
    fake
}

#[test]
fn the_plan_of_each_role_lists_the_agents_it_talks_to() {
    let roster = Roster {
        pods: vec!["harold".into(), "keymap".into()],
        workers: vec![("harold".into(), "decision-tool-a1".into())],
    };
    let titles = |agent: &Agent| -> Vec<String> {
        plan_links(agent, &roster)
            .rows
            .iter()
            .map(|r| r.peer.title())
            .collect()
    };
    assert_eq!(
        titles(&Agent::Chief),
        [
            "pnem harold",
            "pod-coordination harold",
            "pnem keymap",
            "pod-coordination keymap"
        ]
    );
    assert_eq!(titles(&pnem()), ["chief", "pod-coordination harold"]);
    assert_eq!(
        titles(&coordination()),
        ["chief", "pnem harold", "worker decision-tool-a1"]
    );
    assert_eq!(titles(&worker()), ["pod-coordination harold"]);
    let plan = plan_links(&coordination(), &roster);
    assert_eq!(
        plan.header,
        ["", "coordination -> agent", "agent -> coordination"]
    );
    assert_eq!(plan.rows[2].delivery, Delivery::Pane);
    assert_eq!(plan.rows[0].delivery, Delivery::SendMessage);
    assert_eq!(
        plan_links(&Agent::Chief, &roster).header,
        ["pod", "agent", "chief -> agent", "agent -> chief"]
    );
}

#[test]
fn every_link_that_resolves_and_accepts_is_ok_both_ways_and_no_byte_is_sent() {
    let roster = Roster {
        pods: vec!["harold".into()],
        workers: vec![],
    };
    let fake = healthy();
    let result = check_links(&pnem(), &plan_links(&pnem(), &roster), &fake);
    assert!(result.fails.is_empty());
    assert!(
        result
            .table
            .rows
            .iter()
            .all(|r| r.outgoing == Outcome::Ok && r.incoming == Outcome::Ok)
    );
    assert!(!fake.probed.lock().unwrap().is_empty());
}

#[test]
fn a_lookup_that_gives_an_old_agent_fails_that_direction_only() {
    let roster = Roster {
        pods: vec!["harold".into()],
        workers: vec![],
    };
    let mut fake = healthy();
    fake.lookups.insert("chief".into(), resolved("%1", 99));
    let result = check_links(&pnem(), &plan_links(&pnem(), &roster), &fake);
    let row = &result.table.rows[0];
    assert_eq!((row.outgoing, row.incoming), (Outcome::Fail, Outcome::Ok));
    assert!(
        result.fails[0]
            .text
            .starts_with("pnem harold -> chief: old_agent: ")
    );
    assert_eq!(result.table.rows[0].outgoing_reason, "old_agent");
}

#[test]
fn a_missing_a_refused_and_an_unknown_socket_give_fail_fail_and_not_checked() {
    let roster = Roster {
        pods: vec!["harold".into()],
        workers: vec![],
    };
    for (probe, outcome, word) in [
        (SocketProbe::Missing, Outcome::Fail, "socket_missing"),
        (SocketProbe::Refused, Outcome::Fail, "socket_refused"),
        (SocketProbe::Unknown, Outcome::NotChecked, "socket_unknown"),
    ] {
        let mut fake = healthy();
        fake.sockets.insert(100, probe);
        let result = check_links(&pnem(), &plan_links(&pnem(), &roster), &fake);
        let row = &result.table.rows[0];
        assert_eq!(row.outgoing, outcome, "{word}");
        let failed = result.fails.iter().any(|f| f.text.contains(word));
        let unchecked = result.not_checked.iter().any(|f| f.contains(word));
        assert_eq!(
            (failed, unchecked),
            (outcome == Outcome::Fail, outcome == Outcome::NotChecked)
        );
    }
}

#[test]
fn a_not_checked_link_is_no_fail_for_the_result_count_and_carries_its_reason() {
    let roster = Roster {
        pods: vec!["harold".into()],
        workers: vec![],
    };
    let mut fake = healthy();
    fake.sockets.insert(100, SocketProbe::Unknown);
    let result = check_links(&pnem(), &plan_links(&pnem(), &roster), &fake);
    assert!(result.fails.is_empty());
    assert_eq!(result.not_checked, ["pnem harold -> chief: socket_unknown"]);
}

#[test]
fn a_pane_link_needs_the_pane_to_hold_the_receivers_agent_pid() {
    let roster = Roster {
        pods: vec!["harold".into()],
        workers: vec![],
    };
    let mut fake = Fake::default();
    fake.lookups
        .insert("pod-coordination harold".into(), resolved("%3", 300));
    fake.running.insert("pod-coordination harold".into(), 300);
    fake.panes.insert("%3".into(), Ok(Some(300)));
    fake.lookups
        .insert("worker decision-tool-a1".into(), resolved("%4", 400));
    fake.running.insert("worker decision-tool-a1".into(), 400);
    fake.panes.insert("%4".into(), Ok(Some(999)));
    let roster = Roster {
        workers: vec![("harold".into(), "decision-tool-a1".into())],
        ..roster
    };
    let mut plan = plan_links(&coordination(), &roster);
    plan.rows = vec![plan.rows[2].clone()];
    let result = check_links(&coordination(), &plan, &fake);
    assert_eq!(result.table.rows[0].outgoing, Outcome::Fail);
    assert!(
        result
            .fails
            .iter()
            .any(|f| f.text.contains("pane_holds_other_agent"))
    );
    assert!(
        fake.probed.lock().unwrap().is_empty(),
        "a pane link opens no socket"
    );
}

#[test]
fn a_multiplexer_that_cannot_be_read_gives_not_checked_with_its_reason() {
    let roster = Roster {
        pods: vec!["harold".into()],
        workers: vec![("harold".into(), "w".into())],
    };
    let mut fake = Fake::default();
    fake.lookups.insert("worker w".into(), resolved("%4", 400));
    fake.running.insert("worker w".into(), 400);
    fake.panes.insert("%4".into(), Err(()));
    let plan = plan_links(&coordination(), &roster);
    let result = check_links(&coordination(), &plan, &fake);
    assert_eq!(result.table.rows[2].outgoing, Outcome::NotChecked);
    assert!(
        result
            .fails
            .iter()
            .all(|f| !f.text.contains("multiplexer_unreachable"))
    );
    assert!(
        result
            .not_checked
            .iter()
            .any(|f| f.contains("multiplexer_unreachable"))
    );
}

#[test]
fn the_socket_path_is_read_from_the_session_file_and_a_shape_change_is_unknown() {
    assert_eq!(
        socket_path_in(r#"{"pid":1,"messagingSocketPath":"/tmp/a.sock"}"#),
        Some("/tmp/a.sock".into())
    );
    assert_eq!(socket_path_in(r#"{"pid":1}"#), None);
    assert_eq!(socket_path_in(r#"{"messagingSocketPath":7}"#), None);
    assert_eq!(socket_path_in("not json"), None);
}

#[test]
fn the_probe_gives_missing_for_no_path_on_disk_and_refused_for_a_file_that_is_no_socket() {
    let dir = std::env::temp_dir().join(format!("harold-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(probe_socket(None), SocketProbe::Unknown);
    assert_eq!(
        probe_socket(Some(&dir.join("none.sock"))),
        SocketProbe::Missing
    );
    let plain = dir.join("plain.sock");
    std::fs::write(&plain, b"").unwrap();
    assert_eq!(probe_socket(Some(&plain)), SocketProbe::Refused);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_connection_that_never_ends_is_refused_after_the_wait_and_one_that_ends_is_accepted() {
    let dir = std::env::temp_dir().join(format!("harold-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("any.sock");
    std::fs::write(&path, b"").unwrap();
    let started = std::time::Instant::now();
    let probe = probe_with(Some(&path), std::time::Duration::from_millis(50), |_| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        Ok(())
    });
    assert_eq!(probe, SocketProbe::Refused);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let probe = probe_with(Some(&path), std::time::Duration::from_secs(2), |_| Ok(()));
    assert_eq!(probe, SocketProbe::Accepts);
    std::fs::remove_dir_all(&dir).unwrap();
}
