use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::settings::{AgentProviderSettings, AgentSettings, IdleAfterWorking, ScreenAdapter};
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
        idle_after_working: crate::settings::IdleAfterWorking::Idle,
    }])
}

/// A fake `rex` program. It logs each call and answers from files in its folder.
struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rex-hosts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("running")).unwrap();
        let script = format!(
            "D='{}'\necho \"$*\" >> \"$D/calls.log\"\n\
             if [ -f \"$D/delay\" ]; then case \"$*\" in *whoami*) ;; *)\n\
             case \"$*\" in *session.view*) S=view;; *process*) S=process;; *) S=other;; esac\n\
             touch \"$D/running/$$\"\n\
             echo \"$S $(ls \"$D/running\" | wc -l)\" >> \"$D/counts.log\"\n\
             sleep \"$(cat \"$D/delay\")\"\n\
             rm -f \"$D/running/$$\";; esac; fi\n\
             case \"$*\" in\n\
             *whoami*) cat \"$D/whoami.json\";;\n\
             *\" ls --json\"*) cat \"$D/ls.json\";;\n\
             *session.view*) cat \"$D/view.json\";;\n\
             *\"hosts list\"*) cat \"$D/hosts.json\";;\n\
             *\"-b block:b1 process\"*) cat \"$D/process-b1.json\";;\n\
             *\"-b block:b2 process\"*) cat \"$D/process-b2.json\";;\n\
             *\"-b block:b1 program_status\"*) cat \"$D/status-b1.json\";;\n\
             *\" process\"*) cat \"$D/process-b2.json\";;\n\
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

    /// Makes each call, except `whoami`, last this many seconds.
    fn call_takes(&self, seconds: &str) {
        self.put("delay", seconds);
    }

    /// The highest count of calls of one step (`view`, `process` or `other`) that ran together.
    fn peak(&self, step: &str) -> usize {
        let log = std::fs::read_to_string(self.dir.join("counts.log")).unwrap_or_default();
        log.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                (words.next()? == step).then(|| words.next()?.parse().ok())?
            })
            .max()
            .unwrap_or(0)
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
        self.inventory_for(label, mark, clock, settings())
    }

    /// An inventory whose provider `alpha` asks that idle after working shows done.
    fn done_inventory(&self, clock: Arc<AtomicI64>) -> RexHostInventory {
        let AgentSettings(mut providers) = settings();
        providers[0].idle_after_working = IdleAfterWorking::Done;
        self.inventory_for("omarchy", None, clock, AgentSettings(providers))
    }

    fn inventory_for(
        &self,
        label: &str,
        mark: Option<&str>,
        clock: Arc<AtomicI64>,
        settings: AgentSettings,
    ) -> RexHostInventory {
        let path: &Path = &self.dir.join("rex");
        let path = path.to_path_buf();
        RexHostInventory::with_program(
            label.to_string(),
            settings,
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
fn a_host_agent_carries_its_window_name() {
    let fake = Fake::new("window-name");
    fake.put(
        "view.json",
        r#"{"windows":[{"label":"linux-satellite-2b","layers":[{"blocks":[
        {"block_id":"block:b1","creator_name":"com.superlogical.terminal"},
        {"block_id":"block:b2","creator_name":"com.superlogical.terminal"}]}]}]}"#,
    );
    let scan = fake.inventory("omarchy", None, clock(1000)).scan();
    assert_eq!(scan.agents.len(), 1);
    assert_eq!(scan.agents[0].window_name, "linux-satellite-2b");
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
fn the_icon_is_empty_with_no_override_and_the_rex_icon_is_not_used() {
    let fake = Fake::new("icon");
    let inventory = fake.inventory("omarchy", None, clock(0));
    assert_eq!(inventory.icon(), "");
    // The list still tells a known label from an unknown one.
    assert_eq!(inventory.listed(), Some(true));
    assert_eq!(
        fake.inventory("unknown", None, clock(0)).listed(),
        Some(false)
    );
}

#[test]
fn the_mark_override_is_the_icon() {
    let fake = Fake::new("mark");
    let inventory = fake.inventory("omarchy", Some("arch"), clock(0));
    assert_eq!(inventory.icon(), "arch");
    let _ = inventory.scan();
    assert_eq!(inventory.icon(), "arch");
}

#[test]
fn the_host_list_is_read_at_most_once_a_minute_and_a_failed_read_keeps_listed() {
    let fake = Fake::new("listread");
    let time = clock(0);
    let inventory = fake.inventory("omarchy", None, Arc::clone(&time));
    assert_eq!(inventory.listed(), Some(true));
    fake.remove_hosts();
    // Inside the minute, no new read happens.
    time.store(30_000, Ordering::SeqCst);
    assert_eq!(inventory.listed(), Some(true));
    let reads = |fake: &Fake| {
        fake.calls()
            .iter()
            .filter(|c| c.contains("hosts list"))
            .count()
    };
    assert_eq!(reads(&fake), 1);
    // After the minute the read fails and the value stays.
    time.store(61_000, Ordering::SeqCst);
    assert_eq!(inventory.listed(), Some(true));
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
fn host_scan_leaves_out_an_exited_block() {
    let fake = Fake::new("exited");
    fake.put(
        "process-b2.json",
        r#"{"home":"/Users/x","last_exit":{"exit_code":1,"error":"exit status 1"}}"#,
    );
    let scan = fake.inventory("omarchy", None, clock(1)).scan();
    assert_eq!(scan.outcome, Ok(()));
    assert_eq!(scan.agents.len(), 1);
    assert_eq!(scan.agents[0].host_pane_id, "rex:omarchy/block:b1");
}

#[test]
fn host_scan_survives_one_unreadable_block() {
    let fake = Fake::new("unreadable");
    fake.put("process-b2.json", "not json");
    let scan = fake.inventory("omarchy", None, clock(1)).scan();
    assert_eq!(scan.outcome, Ok(()));
    assert_eq!(scan.agents.len(), 1);
    assert_eq!(scan.agents[0].host_pane_id, "rex:omarchy/block:b1");
}

#[test]
fn host_scan_fails_when_every_process_call_fails() {
    let fake = Fake::new("allfail");
    fake.put("process-b1.json", "not json");
    fake.put("process-b2.json", "not json");
    let scan = fake.inventory("omarchy", None, clock(1)).scan();
    assert_eq!(scan.outcome, Err(RexError::Malformed));
    assert!(scan.agents.is_empty());
}

#[test]
fn a_host_with_2_sessions_and_8_blocks_runs_each_step_at_once() {
    let fake = Fake::new("pool");
    fake.put(
        "ls.json",
        r#"{"sessions":[{"session_id":"s1","label":"one"},{"session_id":"s2","label":"two"}]}"#,
    );
    let block = |id: &str| {
        format!(r#"{{"block_id":"block:{id}","creator_name":"com.superlogical.terminal"}}"#)
    };
    let blocks = ["b1", "b2", "b3", "b4"].map(block).join(",");
    fake.put(
        "view.json",
        &format!(r#"{{"windows":[{{"layers":[{{"blocks":[{blocks}]}}]}}]}}"#),
    );
    // Each call stays 0.5 s, so slow starts under load still overlap.
    fake.call_takes("0.5");
    let inventory = fake.inventory("omarchy", None, clock(1));
    let scan = inventory.scan();
    assert_eq!(scan.outcome, Ok(()));
    assert_eq!(scan.agents.len(), 2);
    assert_eq!(scan.agents[0].session_name, "one");
    assert_eq!(scan.agents[1].session_name, "two");
    // A serial step has a peak of 1.
    assert!(fake.peak("view") >= 2, "view peak {}", fake.peak("view"));
    assert!(
        fake.peak("process") >= 4,
        "process peak {}",
        fake.peak("process")
    );
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
fn a_local_call_records_an_empty_host_and_a_remote_call_the_label() {
    assert_eq!(recorded_host(false, "omarchy"), "");
    assert_eq!(recorded_host(true, "omarchy"), "omarchy");
}

/// End-to-end check. The empty host can only be checked as "rises": other tests also record
/// local calls, and the thread-local call log holds no host. The exact check is the test above.
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

const WORKING: HostAgentState = HostAgentState::Observed(ObservedAgentState::Working);
const IDLE: HostAgentState = HostAgentState::Observed(ObservedAgentState::Idle);
const DONE: HostAgentState = HostAgentState::Observed(ObservedAgentState::Done);

/// The state of the agent after one more scan with the record `word`.
fn scan_with(fake: &Fake, inventory: &RexHostInventory, word: &str) -> HostAgentState {
    fake.status(200, word, "");
    inventory.scan().agents.remove(0).state
}

#[test]
fn a_record_that_goes_from_working_to_idle_gives_done_and_done_stays() {
    let fake = Fake::new("done-rule");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    assert_eq!(scan_with(&fake, &inventory, "idle"), DONE);
    assert_eq!(scan_with(&fake, &inventory, "idle"), DONE);
}

#[test]
fn blocked_then_idle_gives_done() {
    let fake = Fake::new("done-blocked");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(
        scan_with(&fake, &inventory, "blocked"),
        HostAgentState::Observed(ObservedAgentState::Blocked)
    );
    assert_eq!(scan_with(&fake, &inventory, "idle"), DONE);
}

#[test]
fn working_again_after_done_gives_working_and_idle_after_that_gives_done_again() {
    let fake = Fake::new("done-again");
    let inventory = fake.done_inventory(clock(1));
    for (word, expected) in [
        ("working", WORKING),
        ("idle", DONE),
        ("working", WORKING),
        ("idle", DONE),
    ] {
        assert_eq!(scan_with(&fake, &inventory, word), expected, "{word}");
    }
}

#[test]
fn an_idle_record_with_no_working_before_it_gives_idle() {
    let fake = Fake::new("done-idle-first");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(scan_with(&fake, &inventory, "idle"), IDLE);
    assert_eq!(scan_with(&fake, &inventory, "idle"), IDLE);
}

#[test]
fn a_new_pid_after_working_gives_idle_not_done() {
    let fake = Fake::new("done-new-pid");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    fake.agent_process(201);
    fake.status(201, "idle", "");
    assert_eq!(inventory.scan().agents.remove(0).state, IDLE);
}

#[test]
fn a_provider_without_the_setting_keeps_idle() {
    let fake = Fake::new("done-not-asked");
    let inventory = fake.inventory("omarchy", None, clock(1));
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    assert_eq!(scan_with(&fake, &inventory, "idle"), IDLE);
}

#[test]
fn a_failed_scan_between_working_and_idle_still_gives_done() {
    let fake = Fake::new("done-failed-scan");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    let listing = std::fs::read_to_string(fake.dir.join("ls.json")).unwrap();
    std::fs::remove_file(fake.dir.join("ls.json")).unwrap();
    assert!(inventory.scan().outcome.is_err());
    fake.put("ls.json", &listing);
    assert_eq!(scan_with(&fake, &inventory, "idle"), DONE);
}

#[test]
fn a_failed_status_call_between_working_and_idle_keeps_the_working_memory() {
    let fake = Fake::new("done-failed-status");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    fake.put("status-b1.json", "not json");
    assert_eq!(
        inventory.scan().agents.remove(0).state,
        HostAgentState::Unknown
    );
    assert_eq!(scan_with(&fake, &inventory, "idle"), DONE);
}

#[test]
fn an_agent_that_leaves_the_block_forgets_its_working() {
    let fake = Fake::new("done-forgets");
    let inventory = fake.done_inventory(clock(1));
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    fake.put(
        "process-b1.json",
        r#"{"child":{"pid":100,"name":"bash","argv0":"bash","cwd":"/home"}}"#,
    );
    assert!(inventory.scan().agents.is_empty());
    fake.agent_process(200);
    assert_eq!(scan_with(&fake, &inventory, "idle"), IDLE);
}

const GAP_LIMIT_MS: u64 = 180_000;

/// Makes the `process` call of the block `b1` fail.
fn break_b1(fake: &Fake) {
    fake.put("process-b1.json", "not json");
}

#[test]
fn a_block_whose_process_call_fails_keeps_its_row_and_state_within_the_limit() {
    let fake = Fake::new("gap-keeps-row");
    let now = clock(1_000);
    let inventory = fake
        .inventory("omarchy", None, Arc::clone(&now))
        .keeping(GAP_LIMIT_MS);
    let first = inventory.scan().agents;
    assert_eq!(first.len(), 1);
    break_b1(&fake);
    now.store(61_000, Ordering::SeqCst);
    let scan = inventory.scan();
    assert_eq!(scan.outcome, Ok(()));
    assert_eq!(scan.agents, first);
    assert_eq!(scan.agents[0].state, WORKING);
    assert_eq!(scan.agents[0].current_step.as_deref(), Some("Edit files"));
}

#[test]
fn a_turn_that_ends_during_the_block_gap_gives_done() {
    let fake = Fake::new("gap-done");
    let now = clock(1_000);
    let inventory = fake.done_inventory(Arc::clone(&now)).keeping(GAP_LIMIT_MS);
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    break_b1(&fake);
    now.store(61_000, Ordering::SeqCst);
    assert_eq!(inventory.scan().agents[0].state, WORKING);
    fake.agent_process(200);
    assert_eq!(scan_with(&fake, &inventory, "idle"), DONE);
}

#[test]
fn a_block_gap_past_the_limit_drops_the_row_and_its_memory() {
    let fake = Fake::new("gap-expires");
    let now = clock(1_000);
    let inventory = fake.done_inventory(Arc::clone(&now)).keeping(GAP_LIMIT_MS);
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    break_b1(&fake);
    now.store(1_000 + GAP_LIMIT_MS as i64 + 1, Ordering::SeqCst);
    let scan = inventory.scan();
    assert_eq!(scan.outcome, Ok(()));
    assert!(scan.agents.is_empty());
    fake.agent_process(200);
    assert_eq!(scan_with(&fake, &inventory, "idle"), IDLE);
}

#[test]
fn the_block_gap_counts_from_the_last_good_read_of_the_block() {
    let fake = Fake::new("gap-from-good");
    let now = clock(0);
    let inventory = fake
        .inventory("omarchy", None, Arc::clone(&now))
        .keeping(GAP_LIMIT_MS);
    assert_eq!(inventory.scan().agents.len(), 1);
    break_b1(&fake);
    now.store(170_000, Ordering::SeqCst);
    assert_eq!(inventory.scan().agents.len(), 1);
    now.store(181_000, Ordering::SeqCst);
    assert!(inventory.scan().agents.is_empty());
}

#[test]
fn a_block_that_leaves_the_slot_list_goes_at_once() {
    let fake = Fake::new("gap-leaves");
    let now = clock(1_000);
    let inventory = fake
        .inventory("omarchy", None, Arc::clone(&now))
        .keeping(GAP_LIMIT_MS);
    assert_eq!(inventory.scan().agents.len(), 1);
    break_b1(&fake);
    fake.put(
        "view.json",
        r#"{"windows":[{"layers":[{"blocks":[
        {"block_id":"block:b2","creator_name":"com.superlogical.terminal"}]}]}]}"#,
    );
    now.store(2_000, Ordering::SeqCst);
    assert!(inventory.scan().agents.is_empty());
}

#[test]
fn a_new_pid_after_a_block_gap_is_a_new_incarnation() {
    let fake = Fake::new("gap-new-pid");
    let now = clock(1_000);
    let inventory = fake.done_inventory(Arc::clone(&now)).keeping(GAP_LIMIT_MS);
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    break_b1(&fake);
    now.store(31_000, Ordering::SeqCst);
    assert_eq!(inventory.scan().agents.len(), 1);
    fake.agent_process(201);
    fake.status(201, "idle", "");
    let agent = inventory.scan().agents.remove(0);
    assert_eq!(agent.state, IDLE);
    assert_eq!(agent.first_seen_ms, 31_000);
}

#[test]
fn a_limit_of_zero_leaves_out_a_block_whose_process_call_fails() {
    let fake = Fake::new("gap-zero");
    let now = clock(1_000);
    let inventory = fake.done_inventory(Arc::clone(&now)).keeping(0);
    assert_eq!(scan_with(&fake, &inventory, "working"), WORKING);
    break_b1(&fake);
    assert!(inventory.scan().agents.is_empty());
    fake.agent_process(200);
    assert_eq!(scan_with(&fake, &inventory, "idle"), IDLE);
}

#[test]
fn the_failed_operation_names_the_first_failing_call() {
    let fake = Fake::new("gap-operation");
    let inventory = fake.inventory("omarchy", None, clock(1));
    assert_eq!(inventory.scan().outcome, Ok(()));
    assert_eq!(inventory.failed_operation(), None);
    break_b1(&fake);
    fake.put("process-b2.json", "not json");
    assert!(inventory.scan().outcome.is_err());
    assert_eq!(inventory.failed_operation(), Some("process"));
    let listing = std::fs::read_to_string(fake.dir.join("view.json")).unwrap();
    std::fs::remove_file(fake.dir.join("view.json")).unwrap();
    assert!(inventory.scan().outcome.is_err());
    assert_eq!(inventory.failed_operation(), Some("layout"));
    fake.put("view.json", &listing);
    std::fs::remove_file(fake.dir.join("ls.json")).unwrap();
    assert!(inventory.scan().outcome.is_err());
    assert_eq!(inventory.failed_operation(), Some("list"));
}
