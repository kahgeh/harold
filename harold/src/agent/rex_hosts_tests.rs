use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::settings::{AgentProviderSettings, AgentSettings, ScreenAdapter};
use crate::test_support::fake_program::{Language, write_fake_program};

use super::domain::{EffectiveAgentState, ObservedAgentState};
use super::rex::RexError;
use super::rex_hosts::*;

fn settings() -> AgentSettings {
    AgentSettings(vec![AgentProviderSettings {
        id: "alpha".into(),
        display_name: "Alpha".into(),
        command_contains: vec!["alpha-agent".into()],
        busy_all: Vec::new(),
        idle_all: Vec::new(),
        summary_line_prefixes: Vec::new(),
        screen_adapter: ScreenAdapter::GenericV1,
        screen_history_lines: 2000,
        recap_adapter: crate::settings::RecapAdapter::None,
    }])
}

/// A fake `rex` program. It logs each call and answers from files in its folder.
struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rex-hosts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "D='{}'\necho \"$*\" >> \"$D/calls.log\"\ncase \"$*\" in\n\
             *whoami*) cat \"$D/whoami.json\";;\n\
             *\" ls --json\"*) cat \"$D/ls.json\";;\n\
             *session.view*) cat \"$D/view.json\";;\n\
             *\"hosts list\"*) cat \"$D/hosts.json\";;\n\
             *\"-b block:b1 process\"*) cat \"$D/process-b1.json\";;\n\
             *\"-b block:b2 process\"*) cat \"$D/process-b2.json\";;\n\
             *\"-b block:b1 program_status\"*) cat \"$D/status-b1.json\";;\n\
             *) exit 1;;\nesac\n",
            dir.display()
        );
        let path = dir.join("rex");
        write_fake_program(&path, Language::Shell, &script);
        std::fs::write(dir.join("calls.log"), "").unwrap();
        let fake = Self { dir };
        fake.put("whoami.json", r#"{"host_id":"host-1"}"#);
        fake.put(
            "ls.json",
            r#"{"sessions":[{"session_id":"s1","label":"work"}]}"#,
        );
        fake.put(
            "view.json",
            r#"{"windows":[{"layers":[{"blocks":[
            {"block_id":"block:b1","creator_name":"com.superlogical.terminal"},
            {"block_id":"block:b2","creator_name":"com.superlogical.terminal"}]}]}]}"#,
        );
        fake.agent_process(200);
        fake.put(
            "process-b2.json",
            r#"{"child":{"pid":300,"name":"bash","argv0":"bash","cwd":"/home"}}"#,
        );
        fake.status(200, "working", "Edit files");
        fake.put(
            "hosts.json",
            r#"{"hosts":[{"host":{"label":"other","icon":"x"}},{"host":{"label":"omarchy","icon":"server.rack"}}]}"#,
        );
        fake
    }

    fn put(&self, file: &str, text: &str) {
        std::fs::write(self.dir.join(file), text).unwrap();
    }

    fn agent_process(&self, pid: u32) {
        self.put(
            "process-b1.json",
            &format!(
                r#"{{"child":{{"pid":100,"name":"bash","argv0":"bash","cwd":"/home"}},"foreground":{{"pid":{pid},"name":"alpha-agent","argv0":"alpha-agent","cwd":"/work/repo"}},"ancestors":[]}}"#
            ),
        );
    }

    fn status(&self, pid: u32, state: &str, msg: &str) {
        self.put(
            "status-b1.json",
            &format!(
                r#"{{"records":[{{"state":"{state}","msg":"{msg}","owner":{{"pid":{pid}}}}}]}}"#
            ),
        );
    }

    fn remove_hosts(&self) {
        std::fs::remove_file(self.dir.join("hosts.json")).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn inventory(
        &self,
        label: &str,
        mark: Option<&str>,
        clock: Arc<AtomicI64>,
    ) -> RexHostInventory {
        let path: &Path = &self.dir.join("rex");
        let path = path.to_path_buf();
        RexHostInventory::with_program(
            label.to_string(),
            settings(),
            mark.map(str::to_string),
            Arc::new(move || clock.load(Ordering::SeqCst)),
            Arc::new(move || Ok::<_, RexError>(path.clone())),
        )
    }
}

fn clock(start: i64) -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(start))
}

#[test]
fn every_host_call_carries_the_label_and_the_limit() {
    let fake = Fake::new("calls");
    let inventory = fake.inventory("omarchy", None, clock(1000));
    let scan = inventory.scan();
    assert_eq!(scan.outcome, Ok(()));
    let calls = fake.calls();
    assert!(calls.len() >= 5, "{calls:?}");
    for call in &calls {
        assert!(call.contains("-S omarchy"), "{call}");
        assert!(call.contains("--timeout 2s"), "{call}");
        assert!(call.contains("--autostart=false"), "{call}");
    }
    assert!(
        calls
            .iter()
            .any(|call| call.contains("block:b1 program_status"))
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.contains("block:b2 program_status"))
    );
}

#[test]
fn an_agent_gets_a_host_pane_id_and_its_place() {
    let fake = Fake::new("ids");
    let scan = fake.inventory("omarchy", None, clock(1000)).scan();
    assert_eq!(scan.host_id.as_deref(), Some("host-1"));
    assert_eq!(scan.agents.len(), 1);
    let agent = &scan.agents[0];
    assert_eq!(agent.host_pane_id, "rex:omarchy/block:b1");
    assert_eq!(agent.host, "omarchy");
    assert_eq!(agent.pid, 200);
    assert_eq!(agent.provider_id, "alpha");
    assert_eq!(agent.provider_display_name, "Alpha");
    assert_eq!(agent.session_name, "work");
    assert_eq!((agent.window_index, agent.pane_index), (1, 1));
    assert_eq!(agent.working_directory, "/work/repo");
}

#[test]
fn a_record_that_says_blocked_done_or_error_gives_that_state() {
    let fake = Fake::new("five-states");
    for (word, expected) in [
        ("blocked", ObservedAgentState::Blocked),
        ("done", ObservedAgentState::Done),
        ("error", ObservedAgentState::Error),
    ] {
        fake.status(200, word, "");
        let agent = fake
            .inventory("omarchy", None, clock(1))
            .scan()
            .agents
            .remove(0);
        assert_eq!(agent.state, HostAgentState::Observed(expected), "{word}");
        assert_eq!(agent.state.effective(), expected.effective(), "{word}");
    }
}

#[test]
fn an_unknown_host_state_is_effectively_unknown() {
    assert_eq!(
        HostAgentState::Unknown.effective(),
        EffectiveAgentState::Unknown
    );
}

#[test]
fn a_record_for_the_pid_gives_its_state_and_step() {
    let fake = Fake::new("record");
    let agent = fake
        .inventory("omarchy", None, clock(1))
        .scan()
        .agents
        .remove(0);
    assert_eq!(
        agent.state,
        HostAgentState::Observed(ObservedAgentState::Working)
    );
    assert_eq!(agent.current_step.as_deref(), Some("Edit files"));
    fake.status(200, "idle", "");
    let agent = fake
        .inventory("omarchy", None, clock(1))
        .scan()
        .agents
        .remove(0);
    assert_eq!(
        agent.state,
        HostAgentState::Observed(ObservedAgentState::Idle)
    );
    assert_eq!(agent.current_step, None);
}

#[test]
fn no_record_or_another_pid_gives_unknown() {
    let fake = Fake::new("unknown");
    fake.put("status-b1.json", r#"{"records":[]}"#);
    let agent = fake
        .inventory("omarchy", None, clock(1))
        .scan()
        .agents
        .remove(0);
    assert_eq!(agent.state, HostAgentState::Unknown);
    assert_eq!(agent.current_step, None);
    fake.status(999, "working", "Other");
    let agent = fake
        .inventory("omarchy", None, clock(1))
        .scan()
        .agents
        .remove(0);
    assert_eq!(agent.state, HostAgentState::Unknown);
    assert_eq!(agent.current_step, None);
}

#[test]
fn the_first_seen_time_stays_and_a_new_pid_is_a_new_incarnation() {
    let fake = Fake::new("incarnation");
    let time = clock(1000);
    let inventory = fake.inventory("omarchy", None, Arc::clone(&time));
    assert_eq!(inventory.scan().agents[0].first_seen_ms, 1000);
    time.store(5000, Ordering::SeqCst);
    assert_eq!(inventory.scan().agents[0].first_seen_ms, 1000);
    fake.agent_process(201);
    fake.status(201, "working", "Next");
    let agent = inventory.scan().agents.remove(0);
    assert_eq!((agent.pid, agent.first_seen_ms), (201, 5000));
}

#[test]
fn the_icon_comes_from_the_host_list_and_a_failed_read_keeps_it() {
    let fake = Fake::new("icon");
    let time = clock(0);
    let inventory = fake.inventory("omarchy", None, Arc::clone(&time));
    assert_eq!(inventory.icon(), "server.rack");
    fake.remove_hosts();
    // Inside the minute, no new read happens.
    time.store(30_000, Ordering::SeqCst);
    assert_eq!(inventory.icon(), "server.rack");
    let reads = |fake: &Fake| {
        fake.calls()
            .iter()
            .filter(|c| c.contains("hosts list"))
            .count()
    };
    assert_eq!(reads(&fake), 1);
    // After the minute the read fails and the value stays.
    time.store(61_000, Ordering::SeqCst);
    assert_eq!(inventory.icon(), "server.rack");
    assert_eq!(reads(&fake), 2);
    let call = fake
        .calls()
        .into_iter()
        .find(|c| c.contains("hosts list"))
        .unwrap();
    assert!(
        call.contains("--timeout 2s") && call.contains("--json"),
        "{call}"
    );
}

#[test]
fn a_host_with_no_icon_gives_an_empty_icon() {
    let fake = Fake::new("noicon");
    fake.put("hosts.json", r#"{"hosts":[{"host":{"label":"omarchy"}}]}"#);
    assert_eq!(fake.inventory("omarchy", None, clock(0)).icon(), "");
}

#[test]
fn the_mark_override_replaces_the_icon_and_reads_nothing() {
    let fake = Fake::new("mark");
    let inventory = fake.inventory("omarchy", Some("arch"), clock(0));
    assert_eq!(inventory.icon(), "arch");
    let _ = inventory.scan();
    assert_eq!(inventory.icon(), "arch");
    assert!(!fake.calls().iter().any(|call| call.contains("hosts list")));
}

#[test]
fn no_call_reads_the_operating_system_of_the_host() {
    let fake = Fake::new("os");
    let inventory = fake.inventory("omarchy", None, clock(0));
    let _ = inventory.scan();
    let _ = inventory.icon();
    for call in fake.calls() {
        for word in ["uname", "os-release", "sw_vers", "run "] {
            assert!(!call.contains(word), "{call}");
        }
    }
}

#[test]
fn calls_count_as_metrics_with_the_host_label() {
    let fake = Fake::new("metrics");
    let count = |operation: &str| {
        crate::telemetry::test_support::counter_value(
            "harold.multiplexer.calls",
            &[
                ("multiplexer", "rex"),
                ("host", "metrics-host"),
                ("operation", operation),
                ("outcome", "ok"),
            ],
        )
        .unwrap_or(0)
    };
    let _ = fake.inventory("metrics-host", None, clock(0)).scan();
    assert_eq!(count("list"), 1);
    assert_eq!(count("layout"), 1);
    assert_eq!(count("process"), 2);
    assert_eq!(count("status"), 1);
}

#[test]
fn a_failed_listing_gives_an_error_and_no_agents() {
    let fake = Fake::new("failed");
    std::fs::remove_file(fake.dir.join("ls.json")).unwrap();
    let scan = fake.inventory("omarchy", None, clock(0)).scan();
    assert!(scan.outcome.is_err());
    assert!(scan.agents.is_empty());
}

#[test]
fn a_failed_status_call_gives_that_agent_unknown_and_keeps_the_others() {
    let fake = Fake::new("statusfail");
    // The fake answers no program_status call for b2, so that one call fails.
    fake.put(
        "process-b2.json",
        r#"{"child":{"pid":101,"name":"bash","argv0":"bash","cwd":"/home"},"foreground":{"pid":300,"name":"alpha-agent","argv0":"alpha-agent","cwd":"/work/two"},"ancestors":[]}"#,
    );
    let scan = fake.inventory("omarchy", None, clock(1)).scan();
    assert_eq!(scan.outcome, Ok(()));
    assert_eq!(scan.agents.len(), 2);
    let first = &scan.agents[0];
    assert_eq!(first.host_pane_id, "rex:omarchy/block:b1");
    assert_eq!(
        first.state,
        HostAgentState::Observed(ObservedAgentState::Working)
    );
    let second = &scan.agents[1];
    assert_eq!(second.host_pane_id, "rex:omarchy/block:b2");
    assert_eq!(second.state, HostAgentState::Unknown);
    assert_eq!(second.current_step, None);
}

#[test]
fn the_host_list_tells_whether_rex_knows_the_label() {
    let fake = Fake::new("listed");
    assert_eq!(
        fake.inventory("omarchy", None, clock(0)).listed(),
        Some(true)
    );
    assert_eq!(
        fake.inventory("nosuch", None, clock(0)).listed(),
        Some(false)
    );
    // A mark override does not stop the check.
    assert_eq!(
        fake.inventory("nosuch", Some("arch"), clock(0)).listed(),
        Some(false)
    );
    fake.remove_hosts();
    assert_eq!(fake.inventory("omarchy", None, clock(0)).listed(), None);
}

fn whoami_calls(fake: &Fake) -> usize {
    fake.calls()
        .iter()
        .filter(|call| call.contains("whoami"))
        .count()
}

#[test]
fn a_failed_whoami_is_tried_again_only_after_30_seconds() {
    let fake = Fake::new("whoami-retry");
    std::fs::remove_file(fake.dir.join("whoami.json")).unwrap();
    let time = clock(1_000_000);
    let inventory = fake.inventory("omarchy", None, Arc::clone(&time));
    // The listing works, so each scan runs: the scans are 1 second apart for 95 seconds.
    for _ in 0..95 {
        let scan = inventory.scan();
        assert_eq!(scan.outcome, Ok(()));
        assert_eq!(scan.host_id, None);
        time.fetch_add(1_000, Ordering::SeqCst);
    }
    // Tries at 0, 30, 60 and 90 seconds.
    assert_eq!(whoami_calls(&fake), 4);
}

#[test]
fn a_good_whoami_is_kept_and_not_called_again() {
    let fake = Fake::new("whoami-kept");
    let time = clock(0);
    let inventory = fake.inventory("omarchy", None, Arc::clone(&time));
    for _ in 0..3 {
        assert_eq!(inventory.scan().host_id.as_deref(), Some("host-1"));
        time.fetch_add(60_000, Ordering::SeqCst);
    }
    assert_eq!(whoami_calls(&fake), 1);
}

#[test]
fn the_local_host_id_comes_from_whoami_with_no_server_flag() {
    let fake = Fake::new("whoami-local");
    fake.put("whoami.json", r#"{"host_id":"mac-id"}"#);
    let inventory = fake.inventory("omarchy", None, clock(0));
    assert_eq!(inventory.local_host_id().as_deref(), Some("mac-id"));
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(!calls[0].contains("-S"), "{calls:?}");
    assert!(calls[0].contains("whoami --json"), "{calls:?}");
    std::fs::remove_file(fake.dir.join("whoami.json")).unwrap();
    assert_eq!(inventory.local_host_id(), None);
}

#[test]
fn the_local_whoami_call_records_an_empty_host() {
    let fake = Fake::new("whoami-metric");
    let count = |host: &str| {
        crate::telemetry::test_support::counter_value(
            "harold.multiplexer.calls",
            &[
                ("multiplexer", "rex"),
                ("host", host),
                ("operation", "other"),
                ("outcome", "ok"),
            ],
        )
        .unwrap_or(0)
    };
    let local_before = count("");
    let inventory = fake.inventory("whoami-metric-host", None, clock(0));
    assert_eq!(inventory.local_host_id().as_deref(), Some("host-1"));
    // Other tests also record local calls, so the empty host only has to rise.
    assert!(count("") > local_before);
    assert_eq!(count("whoami-metric-host"), 0);
    let _ = inventory.scan();
    assert_eq!(count("whoami-metric-host"), 1);
}
