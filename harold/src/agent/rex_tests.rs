use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{Value, json};

use crate::agent::domain::ObservedAgentState;
use crate::settings::{AgentProviderSettings, AgentSettings};

use super::*;
use crate::test_support::fake_program::{Language, write_fake_program};

fn provider(id: &str, command_contains: &[&str]) -> AgentProviderSettings {
    AgentProviderSettings {
        id: id.to_string(),
        display_name: id.to_uppercase(),
        command_contains: command_contains.iter().map(|s| (*s).to_string()).collect(),
        busy_all: Vec::new(),
        idle_all: Vec::new(),
        summary_line_prefixes: Vec::new(),
        screen_adapter: crate::settings::ScreenAdapter::GenericV1,
        recap_adapter: crate::settings::RecapAdapter::None,
        idle_after_working: crate::settings::IdleAfterWorking::Idle,
        screen_history_lines: 2000,
    }
}

fn settings() -> AgentSettings {
    AgentSettings(vec![
        provider("claude", &["claude"]),
        provider("codex", &["codex"]),
    ])
}

fn proc(pid: u32, argv0: &str, script: Option<&str>) -> RexProc {
    RexProc {
        pid,
        name: argv0.to_string(),
        argv0: argv0.to_string(),
        script: script.map(str::to_string),
        cwd: format!("/work/{pid}"),
    }
}

fn slot(block: &str, window: u32, pane: u32) -> RexBlockSlot {
    RexBlockSlot {
        session_id: "session:1".into(),
        session_label: "harold-main".into(),
        window,
        pane,
        block_id: block.into(),
        window_name: String::new(),
    }
}

#[test]
fn pane_ids_have_the_block_form() {
    assert!(valid_rex_pane_id(
        "block:01a18fe6-c4f7-7b52-a9f2-9682b5c097c7"
    ));
    for bad in ["block:", "%47", "block:a b", "blocks:1", "block:a;b", ""] {
        assert!(!valid_rex_pane_id(bad), "{bad}");
    }
}

#[test]
fn control_characters_in_strings_are_escaped_before_parsing() {
    let raw = "{\"data\":\"a\u{1b}[0m\r\nb\",\n \"total\": 3}";
    assert!(serde_json::from_str::<Value>(raw).is_err());
    let value = parse_json(raw).unwrap();
    assert_eq!(value["data"], "a\u{1b}[0m\r\nb");
    assert_eq!(value["total"], 3);
    let escaped = parse_json("{\"q\":\"a\\\"\u{1}b\"}").unwrap();
    assert_eq!(escaped["q"], "a\"\u{1}b");
}

#[test]
fn parse_session_view_keeps_each_window_label() {
    let view = r#"{"windows":[
      {"label":"pod","layers":[{"blocks":[
        {"block_id":"block:a","creator_name":"com.superlogical.terminal"}]}]},
      {"label":"linux-satellite-2b","layers":[{"blocks":[
        {"block_id":"block:b","creator_name":"com.superlogical.terminal"}]}]}]}"#;
    let windows = parse_session_view(view).unwrap();
    let labels: Vec<&str> = windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["pod", "linux-satellite-2b"]);
    assert_eq!(windows[1].blocks, vec!["block:b"]);
}

#[test]
fn a_window_with_no_label_gives_an_empty_name() {
    let view = r#"{"windows":[
      {"layers":[]},
      {"label":"","layers":[]}]}"#;
    let windows = parse_session_view(view).unwrap();
    assert_eq!(windows.len(), 2);
    assert!(windows.iter().all(|w| w.label.is_empty()));
}

#[test]
fn slots_carry_their_window_name() {
    let session = RexSession {
        id: "session:1".into(),
        label: "harold-main".into(),
    };
    let windows = vec![
        RexWindow {
            label: "pod".into(),
            blocks: vec!["block:a".into(), "block:b".into()],
        },
        RexWindow {
            label: String::new(),
            blocks: vec!["block:c".into()],
        },
    ];
    let slots = slots_of(&session, &windows);
    let names: Vec<&str> = slots.iter().map(|s| s.window_name.as_str()).collect();
    assert_eq!(names, ["pod", "pod", ""]);
}

#[test]
fn rex_ls_session_view_and_process_parse() {
    let sessions = parse_sessions(
        r#"{"sessions":[{"session_id":"session:a","label":"one"},{"session_id":"session:b","label":"two"}]}"#,
    )
    .unwrap();
    assert_eq!(sessions[1].label, "two");
    assert_eq!(parse_sessions("nope"), Err(RexError::Malformed));

    let view = r#"{"windows":[
      {"window_id":"w1","layers":[{"kind":"tiled","blocks":[
        {"block_id":"block:a","creator_name":"com.superlogical.terminal"},
        {"block_id":"block:x","creator_name":"com.other.thing"},
        {"block_id":"block:b","creator_name":"com.superlogical.terminal"}]}]},
      {"window_id":"w2","layers":[{"kind":"tiled","blocks":[
        {"block_id":"block:c","creator_name":"com.superlogical.terminal"}]}]}]}"#;
    let windows = parse_session_view(view).unwrap();
    let blocks: Vec<&[String]> = windows.iter().map(|w| w.blocks.as_slice()).collect();
    assert_eq!(blocks, [&["block:a", "block:b"][..], &["block:c"][..]]);
    let session = RexSession {
        id: "session:1".into(),
        label: "harold-main".into(),
    };
    let slots = slots_of(&session, &windows);
    let labels: Vec<String> = slots
        .iter()
        .map(|s| format!("{}:{}.{}={}", s.session_label, s.window, s.pane, s.block_id))
        .collect();
    assert_eq!(
        labels,
        [
            "harold-main:1.1=block:a",
            "harold-main:1.2=block:b",
            "harold-main:2.1=block:c"
        ]
    );

    let process = parse_process(
        r#"{"child":{"pid":1,"name":"login"},
            "foreground":{"pid":3,"name":"2.1.316","argv0":"claude","cwd":"/w"},
            "ancestors":[{"pid":2,"name":"zsh","argv0":"-zsh"},{"pid":1,"name":"login"}]}"#,
    )
    .unwrap();
    assert_eq!(process.foreground.as_ref().unwrap().program(), "claude");
    assert_eq!(process.ancestors[0].program(), "zsh");
    assert_eq!(process.ancestors[1].program(), "login");
}

fn process(child: u32, foreground: Option<RexProc>, ancestors: Vec<RexProc>) -> RexProcess {
    RexProcess {
        child: Some(proc(child, "login", None)),
        foreground,
        ancestors,
    }
}

const EXITED_ANSWER: &str =
    r#"{"home":"/Users/x","last_exit":{"exit_code":1,"error":"exit status 1"}}"#;

fn exited_process() -> RexProcess {
    RexProcess {
        child: None,
        foreground: None,
        ancestors: Vec::new(),
    }
}

#[test]
fn parse_process_of_an_exited_block_has_no_child() {
    let process = parse_process(EXITED_ANSWER).unwrap();
    assert_eq!(process.child, None);
    assert_eq!(process.foreground, None);
    assert!(process.ancestors.is_empty());
}

#[test]
fn parse_process_of_an_exited_block_with_a_null_child_has_no_child() {
    let process =
        parse_process(r#"{"child":null,"last_exit":{"exit_code":1,"error":"exit status 1"}}"#)
            .unwrap();
    assert_eq!(process, exited_process());
}

#[test]
fn parse_process_with_neither_child_nor_last_exit_is_malformed() {
    assert_eq!(parse_process(r#"{"home":"/x"}"#), Err(RexError::Malformed));
}

#[test]
fn parse_process_with_child_and_last_exit_keeps_the_child() {
    let process = parse_process(
        r#"{"child":{"pid":7,"name":"login"},"last_exit":{"exit_code":1,"error":"x"}}"#,
    )
    .unwrap();
    assert_eq!(process.child.unwrap().pid, 7);
}

#[test]
fn find_agent_finds_nothing_in_an_exited_block() {
    assert!(find_agent(&exited_process(), &settings()).is_none());
}

#[test]
fn is_current_is_false_for_an_exited_block() {
    let _directory = directory_test_lock();
    let live = process(
        1,
        Some(proc(5, "claude", None)),
        vec![proc(1, "login", None)],
    );
    let api = Arc::new(fake(&[("block:a", Ok(live))]));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let incarnation = inventory.scan().unwrap()[0].incarnation.clone();
    assert!(inventory.is_current(&incarnation).unwrap());

    api.processes
        .lock()
        .unwrap()
        .insert("block:a".into(), Ok(exited_process()));
    assert_eq!(inventory.is_current(&incarnation), Ok(false));
}

#[test]
fn the_agent_is_found_in_the_foreground_in_ancestors_and_through_a_script() {
    let s = settings();
    let fg = process(
        1,
        Some(proc(5, "claude", None)),
        vec![proc(2, "zsh", None), proc(1, "login", None)],
    );
    assert_eq!(find_agent(&fg, &s).unwrap().0.pid, 5);

    let tool = process(
        1,
        Some(proc(9, "sleep", None)),
        vec![
            proc(5, "claude", None),
            proc(2, "zsh", None),
            proc(1, "login", None),
        ],
    );
    assert_eq!(find_agent(&tool, &s).unwrap().0.pid, 5);

    let node = process(
        1,
        Some(proc(6, "node", Some("/opt/lib/codex"))),
        vec![proc(1, "login", None)],
    );
    let (found, program) = find_agent(&node, &s).unwrap();
    assert_eq!((found.pid, program.as_str()), (6, "codex"));

    let none = process(1, Some(proc(2, "zsh", None)), vec![proc(1, "login", None)]);
    assert!(find_agent(&none, &s).is_none());
}

#[test]
fn the_agent_nearer_the_foreground_wins() {
    let s = settings();
    let nested = process(
        1,
        Some(proc(9, "codex", None)),
        vec![proc(5, "claude", None), proc(1, "login", None)],
    );
    assert_eq!(find_agent(&nested, &s).unwrap().0.pid, 9);
    let in_tool = process(
        1,
        Some(proc(9, "sleep", None)),
        vec![
            proc(7, "codex", None),
            proc(5, "claude", None),
            proc(1, "login", None),
        ],
    );
    assert_eq!(find_agent(&in_tool, &s).unwrap().0.pid, 7);
}

#[test]
fn a_matching_process_above_the_child_is_not_taken() {
    let s = settings();
    let above = process(
        2,
        Some(proc(3, "zsh", None)),
        vec![proc(2, "login", None), proc(1, "claude", None)],
    );
    assert!(find_agent(&above, &s).is_none());
}

#[test]
fn observe_block_builds_the_label_ids_and_folder() {
    let s = settings();
    let p = process(
        1,
        Some(proc(5, "claude", None)),
        vec![proc(1, "login", None)],
    );
    let (observation, program) = observe_block(&slot("block:a", 2, 3), &p, &s, 99, |pid| {
        assert_eq!(pid, 5);
        Some(5_000)
    })
    .unwrap()
    .unwrap();
    assert_eq!(program, "claude");
    assert_eq!(observation.tmux_target, "harold-main:2.3");
    assert_eq!(observation.incarnation.pane_id, "block:a");
    assert_eq!(observation.incarnation.pane_pid, 1);
    assert_eq!(observation.incarnation.agent_pid, 5);
    assert_eq!(observation.incarnation.agent_started_at_ms, 5_000);
    assert_eq!(observation.incarnation.provider_id, "claude");
    assert_eq!(observation.working_directory, "/work/5");
    assert_eq!((observation.window_index, observation.pane_index), (2, 3));
    let missing = observe_block(&slot("block:a", 1, 1), &p, &s, 99, |_| None);
    assert_eq!(missing, Err(InventoryError::MissingProcessStartTime));
}

// -- a fake `rex` ------------------------------------------------------------

#[derive(Default)]
struct FakeRex {
    sessions: Vec<RexSession>,
    windows: Vec<Vec<String>>,
    processes: Mutex<HashMap<String, Result<RexProcess, RexError>>>,
    starts: Mutex<HashMap<u32, i64>>,
    start_reads: Mutex<Vec<u32>>,
    list_error: Option<RexError>,
    calls: Mutex<Vec<(String, Value)>>,
    blob: String,
    chunk: usize,
    pastes: Mutex<Vec<(String, String, String)>>,
    keys: Mutex<Vec<(String, String)>>,
    /// The windows of a session, when they differ from `windows`.
    views: HashMap<String, Vec<Vec<String>>>,
    /// The replies of `program_status` for each block, in call order. The last one repeats.
    /// `None` is a failed call.
    sequences: HashMap<String, Vec<Option<Value>>>,
    status_calls: Mutex<HashMap<String, usize>>,
    /// Holds the first `program_status` call until the test opens the gate.
    gate: Option<Arc<Gate>>,
    /// The purpose of each `read_call`, in order.
    purposes: Mutex<Vec<&'static str>>,
    /// Blocks that `session_view` leaves out.
    hidden: Mutex<Vec<String>>,
    /// The label of each window of `session_view`, by position. A missing one is empty.
    labels: Vec<String>,
}

/// The first `program_status` call for `block` waits and answers `first`. Later calls for
/// the block answer `later`.
struct Gate {
    block: String,
    calls: std::sync::atomic::AtomicUsize,
    open: Mutex<bool>,
    changed: std::sync::Condvar,
    first: Result<Value, RexError>,
    later: Result<Value, RexError>,
}

impl Gate {
    fn new(block: &str, first: Value, later: Value) -> Arc<Self> {
        Self::with(block, Ok(first), Ok(later))
    }

    fn with(
        block: &str,
        first: Result<Value, RexError>,
        later: Result<Value, RexError>,
    ) -> Arc<Self> {
        Arc::new(Self {
            block: block.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            open: Mutex::new(false),
            changed: std::sync::Condvar::new(),
            first,
            later,
        })
    }

    fn call(&self) -> Result<Value, RexError> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n > 0 {
            return self.later.clone();
        }
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
        self.first.clone()
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Opens the gate when it drops, also when the test panics. Declare it after the
    /// `RexStatus`, so it drops first: the drop of `RexStatus` joins the reader threads,
    /// and a thread in a closed gate never ends.
    fn guard(self: &Arc<Self>) -> GateGuard {
        GateGuard(Arc::clone(self))
    }
}

struct GateGuard(Arc<Gate>);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Runs a test body on its own thread. A body that does not end in 30 s fails with the
/// test's name, so a hang costs 30 s and not the 60 minutes of a CI job.
fn within_30s(name: &str, body: impl FnOnce() + Send + 'static) {
    let (done, ended) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            let _ = done.send(());
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        })
        .unwrap();
    if ended.recv_timeout(Duration::from_secs(30)).is_err() {
        panic!("{name} did not end in 30 s");
    }
    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

impl RexApi for FakeRex {
    fn sessions(&self) -> Result<Vec<RexSession>, RexError> {
        match self.list_error {
            Some(error) => Err(error),
            None => Ok(self.sessions.clone()),
        }
    }
    fn session_view(&self, session: &str) -> Result<Vec<RexWindow>, RexError> {
        let mut windows = self.views.get(session).unwrap_or(&self.windows).clone();
        let hidden = self.hidden.lock().unwrap();
        for window in &mut windows {
            window.retain(|block| !hidden.contains(block));
        }
        Ok(windows
            .into_iter()
            .enumerate()
            .map(|(index, blocks)| RexWindow {
                label: self.labels.get(index).cloned().unwrap_or_default(),
                blocks,
            })
            .collect())
    }
    fn process(&self, _: &str, block: &str) -> Result<RexProcess, RexError> {
        self.processes
            .lock()
            .unwrap()
            .get(block)
            .cloned()
            .unwrap_or(Err(RexError::Failed))
    }
    fn start_time_ms(&self, pid: u32) -> Option<i64> {
        self.start_reads.lock().unwrap().push(pid);
        self.starts.lock().unwrap().get(&pid).copied()
    }
    fn block_call(
        &self,
        _: &str,
        block: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, RexError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), args.clone()));
        match method {
            "program_status" => {
                let n = {
                    let mut calls = self.status_calls.lock().unwrap();
                    let n = calls.entry(block.to_string()).or_insert(0);
                    *n += 1;
                    *n - 1
                };
                if let Some(gate) = self.gate.as_ref().filter(|gate| gate.block == block) {
                    return gate.call();
                }
                let sequence = self.sequences.get(block).ok_or(RexError::Failed)?;
                sequence[n.min(sequence.len() - 1)]
                    .clone()
                    .ok_or(RexError::Failed)
            }
            "size" => Ok(json!({"rows": 24, "columns": 80})),
            "format" => Ok(json!({"id": 7, "total": self.blob.len()})),
            "blob_read" => {
                assert_eq!(args["id"], 7);
                let offset = args["offset"].as_u64().unwrap() as usize;
                let end = (offset + self.chunk.min(args["max_bytes"].as_u64().unwrap() as usize))
                    .min(self.blob.len());
                Ok(
                    json!({"id": 7, "data": &self.blob[offset..end], "total": self.blob.len(), "eof": end == self.blob.len()}),
                )
            }
            _ => Err(RexError::Failed),
        }
    }
    fn read_call(
        &self,
        session_id: &str,
        block_id: &str,
        method: &str,
        args: &Value,
        purpose: crate::telemetry::ScreenPurpose,
    ) -> Result<Value, RexError> {
        self.purposes.lock().unwrap().push(purpose.as_str());
        self.block_call(session_id, block_id, method, args)
    }
    fn paste(&self, session: &str, block: &str, text: &str) -> Result<(), RexError> {
        self.pastes
            .lock()
            .unwrap()
            .push((session.into(), block.into(), text.into()));
        Ok(())
    }
    fn send_key(&self, _: &str, block: &str, key: &str) -> Result<(), RexError> {
        self.keys.lock().unwrap().push((block.into(), key.into()));
        Ok(())
    }
}

impl FakeRex {
    fn status_calls(&self, block: &str) -> usize {
        self.status_calls
            .lock()
            .unwrap()
            .get(block)
            .copied()
            .unwrap_or(0)
    }
}

fn fake(blocks: &[(&str, Result<RexProcess, RexError>)]) -> FakeRex {
    let fake = FakeRex {
        sessions: vec![RexSession {
            id: "session:1".into(),
            label: "harold-main".into(),
        }],
        windows: vec![blocks.iter().map(|(id, _)| (*id).to_string()).collect()],
        ..FakeRex::default()
    };
    for (id, p) in blocks {
        fake.processes
            .lock()
            .unwrap()
            .insert((*id).to_string(), p.clone());
    }
    fake.starts.lock().unwrap().insert(5, 5_000);
    fake
}

fn claude_process() -> RexProcess {
    process(
        1,
        Some(proc(5, "claude", None)),
        vec![proc(1, "login", None)],
    )
}

#[test]
fn the_inventory_lists_agents_and_reads_a_start_time_once_per_pid() {
    let _directory = directory_test_lock();
    let api = Arc::new(fake(&[
        ("block:a", Ok(claude_process())),
        (
            "block:b",
            Ok(process(
                8,
                Some(proc(9, "zsh", None)),
                vec![proc(8, "login", None)],
            )),
        ),
    ]));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let first = inventory.scan().unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].tmux_target, "harold-main:1.1");
    inventory.scan().unwrap();
    assert_eq!(*api.start_reads.lock().unwrap(), vec![5]);
    assert_eq!(
        inventory.program(&first[0].incarnation).as_deref(),
        Some("claude")
    );
    assert_eq!(
        inventory
            .resolve("block:a")
            .unwrap()
            .unwrap()
            .incarnation
            .agent_pid,
        5
    );
    assert!(inventory.resolve("block:b").unwrap().is_none());
}

#[test]
fn is_current_passes_for_the_same_pid_and_start_time_in_ancestors_and_fails_otherwise() {
    let _directory = directory_test_lock();
    let in_tool = process(
        1,
        Some(proc(30, "sleep", None)),
        vec![proc(5, "claude", None), proc(1, "login", None)],
    );
    let api = Arc::new(fake(&[("block:a", Ok(in_tool))]));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let observed = inventory.scan().unwrap();
    let incarnation = observed[0].incarnation.clone();
    assert!(inventory.is_current(&incarnation).unwrap());

    api.starts.lock().unwrap().insert(5, 6_000);
    assert!(!inventory.is_current(&incarnation).unwrap());

    api.starts.lock().unwrap().insert(5, 5_000);
    api.processes.lock().unwrap().insert(
        "block:a".into(),
        Ok(process(
            1,
            Some(proc(2, "zsh", None)),
            vec![proc(1, "login", None)],
        )),
    );
    assert!(!inventory.is_current(&incarnation).unwrap());
}

fn incarnation_for(inventory: &RexAgentInventory, block: &str) -> AgentIncarnation {
    let mut incarnation = inventory.scan().unwrap()[0].incarnation.clone();
    incarnation.pane_id = block.to_string();
    incarnation
}

#[test]
fn is_current_answers_false_for_a_tmux_pane_id_without_a_rex_call() {
    let _directory = directory_test_lock();
    let mut api = fake(&[("block:ic1", Ok(claude_process()))]);
    let inventory = RexAgentInventory::new(
        Arc::new(fake(&[("block:ic1", Ok(claude_process()))])),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let incarnation = incarnation_for(&inventory, "%2");
    api.list_error = Some(RexError::Absent);
    let inventory = RexAgentInventory::new(
        Arc::new(api),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.is_current(&incarnation), Ok(false));
}

#[test]
fn is_current_answers_false_for_a_block_a_successful_listing_does_not_hold() {
    let _directory = directory_test_lock();
    let inventory = RexAgentInventory::new(
        Arc::new(fake(&[("block:ic2", Ok(claude_process()))])),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let incarnation = incarnation_for(&inventory, "block:ic2gone");
    assert_eq!(inventory.is_current(&incarnation), Ok(false));
}

#[test]
fn is_current_answers_false_when_process_fails_and_the_block_is_gone() {
    let _directory = directory_test_lock();
    let first = RexAgentInventory::new(
        Arc::new(fake(&[("block:ic3", Ok(claude_process()))])),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let incarnation = incarnation_for(&first, "block:ic3");
    // The listing no longer holds the block, and its process call fails.
    let api = fake(&[("block:ic3other", Ok(claude_process()))]);
    api.processes
        .lock()
        .unwrap()
        .insert("block:ic3".into(), Err(RexError::Failed));
    let inventory = RexAgentInventory::new(
        Arc::new(api),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.is_current(&incarnation), Ok(false));
}

#[test]
fn is_current_stays_an_error_when_the_listing_fails() {
    let _directory = directory_test_lock();
    for (error, expected) in [
        (RexError::Failed, InventoryError::RexFailed),
        (RexError::Timeout, InventoryError::RexTimeout),
    ] {
        let first = RexAgentInventory::new(
            Arc::new(fake(&[("block:ic4", Ok(claude_process()))])),
            settings(),
            inert_status(),
            WindowNames::default(),
        );
        let incarnation = incarnation_for(&first, "block:ic4unknown");
        let mut api = fake(&[]);
        api.list_error = Some(error);
        let inventory = RexAgentInventory::new(
            Arc::new(api),
            settings(),
            inert_status(),
            WindowNames::default(),
        );
        assert_eq!(inventory.is_current(&incarnation), Err(expected));
    }
}

#[test]
fn is_current_stays_an_error_when_process_fails_and_the_block_is_still_listed() {
    let _directory = directory_test_lock();
    let api = Arc::new(fake(&[("block:ic5", Ok(claude_process()))]));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let incarnation = incarnation_for(&inventory, "block:ic5");
    api.processes
        .lock()
        .unwrap()
        .insert("block:ic5".into(), Err(RexError::Failed));
    assert_eq!(
        inventory.is_current(&incarnation),
        Err(InventoryError::RexFailed)
    );
}

/// Three blocks: `block:a` and `block:c` hold agents, `block:b` answers as given.
fn three_blocks(middle: Result<RexProcess, RexError>) -> FakeRex {
    let other = process(
        2,
        Some(proc(6, "claude", None)),
        vec![proc(2, "login", None)],
    );
    let api = fake(&[
        ("block:a", Ok(claude_process())),
        ("block:b", middle),
        ("block:c", Ok(other)),
    ]);
    api.starts.lock().unwrap().insert(6, 6_000);
    api
}

fn set_block(api: &FakeRex, block: &str, answer: Result<RexProcess, RexError>) {
    api.processes.lock().unwrap().insert(block.into(), answer);
}

#[test]
fn scan_with_an_exited_block_finds_the_other_agents() {
    let _directory = directory_test_lock();
    let (logs, _guard) = capture_logs();
    let inventory = RexAgentInventory::new(
        Arc::new(three_blocks(Ok(exited_process()))),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let observed = inventory.scan().unwrap();
    assert_eq!(observed.len(), 2);
    assert!(logs.with("agent pane unreadable").is_empty());
}

#[test]
fn scan_of_only_an_exited_block_gives_no_row_and_no_error() {
    let _directory = directory_test_lock();
    let api = fake(&[("block:a", Ok(exited_process()))]);
    let inventory = RexAgentInventory::new(
        Arc::new(api),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.scan(), Ok(Vec::new()));
}

#[test]
fn scan_with_one_failing_block_logs_unreadable_once() {
    let _directory = directory_test_lock();
    let (logs, _guard) = capture_logs();
    let api = Arc::new(three_blocks(Err(RexError::Failed)));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    for _ in 0..3 {
        assert_eq!(inventory.scan().unwrap().len(), 2);
    }
    let unreadable = logs.with("agent pane unreadable");
    assert_eq!(unreadable.len(), 1, "{unreadable:?}");
    assert!(unreadable[0].contains("pane_id=block:b"), "{unreadable:?}");
    assert!(
        unreadable[0].contains("reason=\"failed\""),
        "{unreadable:?}"
    );
    assert!(logs.with("agent pane readable again").is_empty());

    set_block(&api, "block:b", Ok(exited_process()));
    assert_eq!(inventory.scan().unwrap().len(), 2);
    let again = logs.with("agent pane readable again");
    assert_eq!(again.len(), 1, "{again:?}");
    assert!(again[0].contains("pane_id=block:b"), "{again:?}");
    assert_eq!(logs.with("agent pane unreadable").len(), 1);
}

#[test]
fn scan_reason_names_timeout_and_malformed() {
    let _directory = directory_test_lock();
    let (logs, _guard) = capture_logs();
    for error in [RexError::Timeout, RexError::Malformed] {
        let inventory = RexAgentInventory::new(
            Arc::new(three_blocks(Err(error))),
            settings(),
            inert_status(),
            WindowNames::default(),
        );
        assert_eq!(inventory.scan().unwrap().len(), 2);
    }
    let lines = logs.with("agent pane unreadable");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("reason=\"timeout\""), "{lines:?}");
    assert!(lines[1].contains("reason=\"malformed\""), "{lines:?}");
}

#[test]
fn scan_fails_when_every_process_call_fails() {
    let _directory = directory_test_lock();
    let api = fake(&[
        ("block:a", Err(RexError::Failed)),
        ("block:b", Err(RexError::Failed)),
        ("block:c", Err(RexError::Failed)),
    ]);
    let inventory = RexAgentInventory::new(
        Arc::new(api),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.scan(), Err(InventoryError::RexFailed));
}

#[test]
fn scan_with_no_blocks_succeeds_with_no_rows() {
    let _directory = directory_test_lock();
    let inventory = RexAgentInventory::new(
        Arc::new(fake(&[])),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.scan(), Ok(Vec::new()));
}

#[test]
fn scan_stopped_during_process_calls_logs_no_unreadable() {
    let _directory = directory_test_lock();
    let (logs, _guard) = capture_logs();
    let inventory = RexAgentInventory::new(
        Arc::new(three_blocks(Err(RexError::Failed))),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let signal = StopSignal::default();
    signal.request();
    let result = run_stoppable(signal, || inventory.scan());
    // One block fails and two answer. Only the stop check turns this into a failed scan
    // that logs nothing per block.
    assert_eq!(result, Err(InventoryError::RexFailed));
    assert!(logs.with("agent pane unreadable").is_empty());
}

#[test]
fn unreadable_set_drops_a_block_that_leaves_the_listing() {
    let _directory = directory_test_lock();
    let (logs, _guard) = capture_logs();
    let api = Arc::new(three_blocks(Err(RexError::Failed)));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.scan().unwrap().len(), 2);

    api.hidden.lock().unwrap().push("block:b".into());
    assert_eq!(inventory.scan().unwrap().len(), 2);

    api.hidden.lock().unwrap().clear();
    assert_eq!(inventory.scan().unwrap().len(), 2);
    assert_eq!(logs.with("agent pane unreadable").len(), 2);
    assert!(logs.with("agent pane readable again").is_empty());
}

#[test]
fn a_failed_listing_or_an_absent_rex_fails_the_scan() {
    let _directory = directory_test_lock();
    for (error, expected) in [
        (RexError::Timeout, InventoryError::RexTimeout),
        (RexError::Absent, InventoryError::RexAbsent),
        (RexError::Failed, InventoryError::RexFailed),
    ] {
        let mut api = fake(&[]);
        api.list_error = Some(error);
        let inventory = RexAgentInventory::new(
            Arc::new(api),
            settings(),
            inert_status(),
            WindowNames::default(),
        );
        assert_eq!(inventory.scan(), Err(expected));
    }
    let api = fake(&[("block:a", Err(RexError::Timeout))]);
    let inventory = RexAgentInventory::new(
        Arc::new(api),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.scan(), Err(InventoryError::RexTimeout));
}

#[test]
fn the_tail_read_calls_blob_read_with_the_id_until_eof() {
    let mut api = fake(&[]);
    api.blob = (0..400).map(|n| format!("line {n}\n")).collect();
    api.chunk = 1000;
    let tail = tail_read(
        &api,
        "session:1",
        "block:a",
        "text",
        2500,
        crate::telemetry::ScreenPurpose::State,
    )
    .unwrap();
    let calls = api.calls.lock().unwrap();
    let reads: Vec<&Value> = calls
        .iter()
        .filter(|c| c.0 == "blob_read")
        .map(|c| &c.1)
        .collect();
    assert!(reads.len() >= 3, "{}", reads.len());
    assert!(reads.iter().all(|args| args["id"] == 7));
    assert_eq!(calls[0].0, "format");
    assert_eq!(calls[0].1["max_bytes"], 1);
    assert_eq!(tail.total as usize, api.blob.len());
    assert!(tail.cut);
    assert!(tail.text.len() <= 2500);
    assert!(api.blob.ends_with(&tail.text));
    assert!(last_lines(&tail, 3).ends_with("line 399"));
    assert_eq!(last_lines(&tail, 3).lines().count(), 3);
    // The first line of a cut tail goes: the kept text starts after the tail's first break.
    let all = last_lines(&tail, 10_000);
    let first_break = tail.text.find('\n').unwrap();
    assert!(tail.text[first_break + 1..].starts_with(&all[..all.find('\n').unwrap()]));
}

#[test]
fn the_styled_read_of_a_writer_passes_the_purpose_writer() {
    let mut api = fake(&[("block:a", Err(RexError::Failed))]);
    api.blob = "first\nsecond\n".into();
    api.chunk = 1 << 20;
    let text = read_screen_styled_through(&api, "block:a");
    assert_eq!(text.as_deref(), Some("first\nsecond"));
    let purposes = api.purposes.lock().unwrap().clone();
    assert_eq!(purposes, vec!["writer", "writer"]);
}

#[test]
fn a_short_output_starts_at_zero_and_keeps_its_first_line() {
    let mut api = fake(&[]);
    api.blob = "first\nsecond\nthird\n".into();
    api.chunk = 1 << 20;
    let tail = tail_read(
        &api,
        "session:1",
        "block:a",
        "text",
        65536,
        crate::telemetry::ScreenPurpose::State,
    )
    .unwrap();
    assert!(!tail.cut);
    assert_eq!(last_lines(&tail, 24), "first\nsecond\nthird");
    assert_eq!(last_lines(&tail, 2), "second\nthird");
}

#[test]
fn tail_size_is_the_screen_at_four_bytes_and_at_least_64_kib() {
    assert_eq!(BlockSize { rows: 24, cols: 80 }.tail_bytes(), 65536);
    assert_eq!(
        BlockSize {
            rows: 100,
            cols: 400
        }
        .tail_bytes(),
        160_000
    );
    assert_eq!(
        parse_size(&json!({"columns": 80, "rows": 24})).unwrap(),
        BlockSize { rows: 24, cols: 80 }
    );
}

#[test]
fn a_paste_event_escapes_a_quote_and_a_line_break() {
    let text = "say \"hi\"\nthen \\ go";
    let event = paste_event_json("session:1", "block:a", text);
    let value: Value = serde_json::from_str(&event).unwrap();
    assert_eq!(value["session_id"], "session:1");
    assert_eq!(value["block_id"], "block:a");
    assert_eq!(value["event"]["type"], "paste");
    assert_eq!(value["event"]["payload"]["text"], text);
    assert!(!event.contains('\n'));
}

#[test]
fn key_names_map_to_rex_names() {
    assert_eq!(rex_key_name("BSpace"), "backspace");
    assert_eq!(rex_key_name("Enter"), "enter");
    assert_eq!(rex_key_name("i"), "i");
    assert_eq!(rex_key_name("x"), "x");
}

#[test]
fn rex_is_looked_for_in_the_documented_order() {
    let apps = vec![PathBuf::from("/Applications/Rex Beta.app")];
    let found = rex_candidates(
        Some("/set/rex"),
        Some(Path::new("/Users/me")),
        Some("/usr/bin:/opt/bin"),
        &apps,
    );
    assert_eq!(found[0], PathBuf::from("/set/rex"));
    assert!(found[1].ends_with("RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex"));
    assert_eq!(found[2], PathBuf::from("/usr/bin/rex"));
    assert_eq!(found[3], PathBuf::from("/opt/bin/rex"));
    assert_eq!(
        found[4],
        PathBuf::from(
            "/Applications/Rex Beta.app/Contents/Helpers/Rex Server.rex/Contents/MacOS/rex"
        )
    );
    assert_eq!(
        rex_candidates(Some(" "), None, None, &[]),
        Vec::<PathBuf>::new()
    );
}

#[test]
#[cfg_attr(
    not(feature = "timing-tests"),
    ignore = "timing: run with --features timing-tests"
)]
fn an_absent_program_is_absent_and_a_slow_one_times_out() {
    let missing = run_program(
        Path::new("/nonexistent/rex"),
        &["ls"],
        Duration::from_secs(1),
    );
    assert_eq!(missing, Err(RexError::Absent));
    // A shell script that ignores the `--autostart=false` flag and sleeps.
    let dir = std::env::temp_dir().join(format!("rex-timeout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("slow-rex.sh");
    write_fake_program(&script, Language::Shell, "sleep 5");
    let started = Instant::now();
    let slow = run_program(&script, &["ls"], Duration::from_millis(100));
    assert_eq!(slow, Err(RexError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(4));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lstart_parses_as_utc() {
    assert_eq!(
        parse_lstart("Sat Oct 31 22:25:16 2026\n"),
        Some(1_793_485_516_000)
    );
    assert_eq!(parse_lstart("garbage"), None);
}

#[test]
fn typing_ports_use_the_session_of_the_block() {
    let _directory = directory_test_lock();
    let api = fake(&[("block:z", Ok(claude_process()))]);
    let inventory = RexAgentInventory::new(
        Arc::new(fake(&[("block:z", Ok(claude_process()))])),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    inventory.scan().unwrap();
    assert_eq!(session_of(&api, "block:z").unwrap(), "session:1");
    assert!(session_of(&api, "block:nope").is_err());
    assert!(block_is_gone(&api, "block:nope"));
    assert!(!block_is_gone(&api, "block:z"));
}

#[test]
fn a_pane_id_of_the_other_form_is_told_apart_from_a_bad_one() {
    use crate::agent::runtime::{pane_id_fits_multiplexer, pane_id_is_other_multiplexer};
    // The test settings are not initialised or name tmux, so a tmux id fits.
    assert!(pane_id_fits_multiplexer("%47"));
    assert!(!pane_id_fits_multiplexer("block:abc"));
    assert!(pane_id_is_other_multiplexer("block:abc"));
    assert!(!pane_id_is_other_multiplexer("%47"));
    // An empty or malformed id is not "the other multiplexer": it keeps its old handling.
    for bad in ["", "47", "block:", "x"] {
        assert!(!pane_id_is_other_multiplexer(bad), "{bad}");
    }
}

/// A `rex` shell script. `rules` come first as `case` arms and the default arms follow.
fn fake_rex(name: &str, rules: &str) -> SystemRex {
    let dir = std::env::temp_dir().join(format!("rex-record-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("rex");
    let body = format!(
        "case \"$*\" in\n{rules}\n*\" ls --json\"*) echo '{{\"sessions\":[]}}';;\n*session.view*) echo '{{\"windows\":[]}}';;\n*\" process\"*) echo '{{\"child\":{{\"pid\":1,\"name\":\"login\"}}}}';;\n*) echo '{{}}';;\nesac\n"
    );
    write_fake_program(&script, Language::Shell, &body);
    SystemRex::new(Some(script.to_string_lossy().into_owned()))
}

/// Runs `call` and returns how many calls of the pair this thread recorded during it.
fn rise<T>(operation: &str, outcome: &str, call: impl FnOnce() -> T) -> (T, usize) {
    crate::telemetry::test_support::noted_calls(operation, outcome, call)
}

#[test]
fn each_rex_call_is_counted_as_ok_under_its_operation() {
    let rex = fake_rex("ok", "");
    let args = json!({});
    let (ok, risen) = rise("list", "ok", || rex.sessions().is_ok());
    assert!(ok);
    assert_eq!(risen, 1);
    let (ok, risen) = rise("layout", "ok", || rex.session_view("s").is_ok());
    assert!(ok);
    assert_eq!(risen, 1);
    let (ok, risen) = rise("process", "ok", || rex.process("s", "b").is_ok());
    assert!(ok);
    assert_eq!(risen, 1);
    for (method, operation) in [
        ("size", "screen_size"),
        ("format", "other"),
        ("blob_read", "other"),
        ("program_status", "status"),
        ("frobnicate", "other"),
    ] {
        let (result, risen) = rise(operation, "ok", || rex.block_call("s", "b", method, &args));
        assert!(result.is_ok(), "{method}");
        assert_eq!(risen, 1, "{method}");
    }
    let (ok, risen) = rise("type", "ok", || rex.paste("s", "b", "x").is_ok());
    assert!(ok);
    assert_eq!(risen, 1);
    let (ok, risen) = rise("key", "ok", || rex.send_key("s", "b", "enter").is_ok());
    assert!(ok);
    assert_eq!(risen, 1);
}

#[test]
fn a_read_call_records_its_purpose_and_a_process_call_records_none() {
    use crate::telemetry::{ScreenPurpose, take_noted_calls};
    let rex = fake_rex("purpose", "");
    let args = json!({});
    for purpose in [
        ScreenPurpose::State,
        ScreenPurpose::Prompt,
        ScreenPurpose::Writer,
    ] {
        for method in ["format", "blob_read"] {
            take_noted_calls();
            assert!(rex.read_call("s", "b", method, &args, purpose).is_ok());
            assert_eq!(
                take_noted_calls(),
                vec![("screen_read", "ok", Some(purpose.as_str()))],
                "{method}"
            );
        }
    }
    take_noted_calls();
    assert!(rex.process("s", "b").is_ok());
    assert_eq!(take_noted_calls(), vec![("process", "ok", None)]);
}

#[test]
fn a_missing_rex_program_is_the_outcome_absent() {
    let rex = SystemRex::new(Some("/nonexistent/harold-test-rex".to_string())).finding_nothing();
    let (result, absent) = rise("list", "absent", || rex.sessions());
    assert_eq!(result, Err(RexError::Absent));
    assert_eq!(absent, 1);
}

#[test]
fn a_failed_size_call_is_screen_size_failed() {
    let rex = fake_rex("size-fail", "*\" size\"*) exit 1;;");
    let (result, risen) = rise("screen_size", "failed", || {
        rex.block_call("s", "b", "size", &json!({}))
    });
    assert_eq!(result, Err(RexError::Failed));
    assert_eq!(risen, 1);
}

#[test]
fn a_slow_status_call_is_status_timeout() {
    let rex = fake_rex("status-slow", "*program_status*) sleep 3;;")
        .with_timeout(Duration::from_millis(100));
    let (result, risen) = rise("status", "timeout", || {
        rex.block_call("s", "b", "program_status", &json!({}))
    });
    assert_eq!(result, Err(RexError::Timeout));
    assert_eq!(risen, 1);
}

#[test]
fn the_version_call_is_counted_under_other() {
    let rex = fake_rex("version-ok", "");
    let (result, risen) = rise("other", "ok", || rex.version());
    assert!(result.is_ok());
    assert_eq!(risen, 1);
}

#[test]
fn a_call_that_harold_stops_is_recorded_as_timeout() {
    let signal = StopSignal::default();
    signal.request();
    let (result, risen) = rise("key", "timeout", || {
        run_stoppable(signal, || {
            recorded(MultiplexerOperation::Key, || -> Result<(), RexError> {
                Err(RexError::Failed)
            })
        })
    });
    assert_eq!(result, Err(RexError::Failed));
    assert_eq!(risen, 1);
}

#[test]
fn a_stopped_ls_is_list_timeout_and_still_gives_absent() {
    let rex = fake_rex("ls-stopped", "*\" ls --json\"*) sleep 3;;");
    let signal = StopSignal::default();
    signal.request();
    let (result, risen) = rise("list", "timeout", || {
        run_stoppable(signal, || rex.sessions())
    });
    assert_eq!(result.err(), Some(RexError::Absent));
    assert_eq!(risen, 1);
}

#[test]
fn the_recorder_maps_an_absent_program_to_absent() {
    let (result, risen) = rise("key", "absent", || {
        recorded(MultiplexerOperation::Key, || -> Result<(), RexError> {
            Err(RexError::Absent)
        })
    });
    assert_eq!(result, Err(RexError::Absent));
    assert_eq!(risen, 1);
}

#[test]
fn a_reply_that_is_not_json_is_process_malformed() {
    let rex = fake_rex("process-bad", "*\" process\"*) echo nope;;");
    let (result, risen) = rise("process", "malformed", || rex.process("s", "b"));
    assert_eq!(result.err(), Some(RexError::Malformed));
    assert_eq!(risen, 1);
}

#[test]
fn a_failed_ls_is_list_absent() {
    let rex = fake_rex("ls-fail", "*\" ls --json\"*) exit 1;;");
    let (result, risen) = rise("list", "absent", || rex.sessions());
    assert_eq!(result.err(), Some(RexError::Absent));
    assert_eq!(risen, 1);
}

/// A fake of 3 sessions that hold 14 blocks, none of them an agent.
fn fake_of_3_sessions_and_14_blocks() -> FakeRex {
    let mut fake = FakeRex::default();
    for (index, blocks) in [5, 5, 4].into_iter().enumerate() {
        let session = format!("session:{index}");
        fake.sessions.push(RexSession {
            id: session.clone(),
            label: format!("label-{index}"),
        });
        let ids: Vec<String> = (0..blocks).map(|n| format!("block:{index}-{n}")).collect();
        for id in &ids {
            fake.processes.lock().unwrap().insert(
                id.clone(),
                Ok(process(
                    8,
                    Some(proc(9, "zsh", None)),
                    vec![proc(8, "login", None)],
                )),
            );
        }
        fake.views.insert(session, vec![ids]);
    }
    fake
}

#[test]
fn a_good_listing_notes_its_sessions_and_panes_and_a_failed_one_notes_nothing() {
    let _directory = directory_test_lock();
    crate::telemetry::take_noted_listings();
    let inventory = RexAgentInventory::new(
        Arc::new(fake_of_3_sessions_and_14_blocks()),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert_eq!(inventory.scan().unwrap().len(), 0);
    assert_eq!(crate::telemetry::take_noted_listings(), vec![(3, 14)]);
    let failing = FakeRex {
        list_error: Some(RexError::Failed),
        ..FakeRex::default()
    };
    let inventory = RexAgentInventory::new(
        Arc::new(failing),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    assert!(inventory.scan().is_err());
    assert_eq!(crate::telemetry::take_noted_listings(), vec![]);
}

// Fixture layout of `rex block call … program_status` (captured from a live Rex block,
// pids and ids changed).
fn status_text(root: &str) -> String {
    format!(
        r#"{{"records":[{root},{{"id":"ad20e9f103fa84d1a","state":"working","app":"x","title":"t","updated_at":"2026-11-01T21:59:39+11:00","owner":{{"pid":7,"name":"2.1.316"}}}}]}}"#
    )
}

fn root_record(state: &str, pid: u32, msg: Option<&str>) -> String {
    let msg = msg.map_or_else(String::new, |m| format!(r#","msg":{}"#, json!(m)));
    format!(
        r#"{{"state":"{state}","app":"x","effective_app":"x","updated_at":"2026-11-01T21:59:40+11:00","owner":{{"pid":{pid},"name":"2.1.316"}}{msg}}}"#
    )
}

#[test]
fn the_root_record_gives_state_pid_and_cleaned_msg() {
    let long = format!("step\n\u{7}{}", "a".repeat(2048));
    let record = parse_program_status(&status_text(&root_record("working", 13934, Some(&long))))
        .expect("parses")
        .expect("has a root");
    assert_eq!(record.state, RecordState::Working);
    assert_eq!(record.pid, 13934);
    let msg = record.msg.expect("has a step");
    assert!(msg.chars().count() <= 160);
    assert!(msg.starts_with("step "));
    assert!(!msg.contains('\n') && !msg.contains('\u{7}'));
}

#[test]
fn a_child_record_is_not_the_root() {
    let text = r#"{"records":[{"id":"x","state":"working","owner":{"pid":5}}]}"#;
    assert_eq!(parse_program_status(text), Ok(None));
    assert_eq!(parse_program_status(r#"{"records":[]}"#), Ok(None));
    assert_eq!(parse_program_status("not json"), Err(RexError::Malformed));
}

#[test]
fn a_record_with_an_unknown_state_word_is_unusable() {
    let text = status_text(&root_record("sleeping", 5, Some("x")));
    assert_eq!(parse_program_status(&text), Ok(None));
    let no_pid = r#"{"records":[{"state":"working","msg":"x"}]}"#;
    assert_eq!(parse_program_status(no_pid), Ok(None));
}

#[test]
fn done_idle_and_error_records_give_no_step() {
    for word in ["done", "idle", "error"] {
        let text = status_text(&root_record(word, 5, Some("a step")));
        let record = parse_program_status(&text).unwrap().unwrap();
        assert_eq!(record.msg, None, "{word}");
        let expected = match word {
            "done" => ObservedAgentState::Done,
            "error" => ObservedAgentState::Error,
            _ => ObservedAgentState::Idle,
        };
        assert_eq!(record.state.observed(), expected, "{word}");
    }
    let text = status_text(&root_record("working", 5, None));
    assert_eq!(parse_program_status(&text).unwrap().unwrap().msg, None);
}

#[test]
fn blocked_maps_to_blocked_and_gives_its_step() {
    let text = status_text(&root_record("blocked", 5, Some("Allow Bash?")));
    let record = parse_program_status(&text).unwrap().unwrap();
    assert_eq!(record.state, RecordState::Blocked);
    assert_eq!(record.state.observed(), ObservedAgentState::Blocked);
    assert_eq!(record.msg.as_deref(), Some("Allow Bash?"));
}

// No status event arrived while the shapes were captured (20 s on a live session: only
// `client_connected` and `client_disconnected` events). The layout below is assumed: a
// `session_event` envelope whose `event` holds `type` and `payload` with `block_id` and,
// for a change, the `record`.
fn event_line(kind: &str, payload: &Value) -> String {
    json!({
        "type": "session_event",
        "payload": {
            "session_id": "session:s1",
            "event": {"type": kind, "payload": payload},
        }
    })
    .to_string()
}

#[test]
fn event_lines_parse_and_bad_lines_are_skipped() {
    let root: Value = serde_json::from_str(&root_record("working", 5, Some("go"))).unwrap();
    let changed = event_line(
        "program_status_changed",
        &json!({"block_id": "block:b1", "record": root}),
    );
    assert_eq!(
        parse_event_line(&changed),
        Some(StatusEvent::Changed {
            block_id: "block:b1".to_string(),
        })
    );
    let mut child = root.clone();
    child["id"] = json!("abc");
    let child_line = event_line(
        "program_status_changed",
        &json!({"block_id": "block:b1", "record": child}),
    );
    assert_eq!(parse_event_line(&child_line), None);
    let removed = event_line(
        "program_status_removed",
        &json!({"block_id": "block:b1", "record": {"state": "done"}}),
    );
    assert_eq!(
        parse_event_line(&removed),
        Some(StatusEvent::Removed {
            block_id: "block:b1".to_string()
        })
    );
    let removed_child = event_line(
        "program_status_removed",
        &json!({"block_id": "block:b1", "record": {"id": "abc"}}),
    );
    assert_eq!(parse_event_line(&removed_child), None);
    let closed = event_line("block_closed", &json!({"block_id": "block:b1"}));
    assert_eq!(
        parse_event_line(&closed),
        Some(StatusEvent::BlockClosed {
            block_id: "block:b1".to_string()
        })
    );
    assert_eq!(parse_event_line("not json"), None);
    assert_eq!(parse_event_line(""), None);
    assert_eq!(
        parse_event_line(&event_line("client_connected", &json!({}))),
        None
    );
    let unusable = event_line(
        "program_status_changed",
        &json!({"block_id": "block:b1", "record": {"state": "later", "owner": {"pid": 1}}}),
    );
    assert_eq!(
        parse_event_line(&unusable),
        Some(StatusEvent::Changed {
            block_id: "block:b1".to_string(),
        })
    );
}

#[test]
fn current_steps_notify_only_on_a_change() {
    let steps = CurrentSteps::default();
    let rx = steps.subscribe();
    assert_eq!(*rx.borrow(), 0);
    steps.set("block:a", 5, Some("one".to_string()));
    assert_eq!(*rx.borrow(), 1);
    steps.set("block:a", 5, Some("one".to_string()));
    assert_eq!(*rx.borrow(), 1);
    assert_eq!(steps.get("block:a", 5).as_deref(), Some("one"));
    assert_eq!(steps.get("block:a", 6), None);
    steps.set("block:a", 5, Some("two".to_string()));
    assert_eq!(*rx.borrow(), 2);
    steps.set("block:b", 5, None);
    assert_eq!(*rx.borrow(), 2);
    steps.clear_block("block:a");
    assert_eq!(*rx.borrow(), 3);
    steps.clear_block("block:a");
    assert_eq!(*rx.borrow(), 3);
    assert_eq!(steps.get("block:a", 5), None);
}

// A live line, captured from Rex 2026-11-01 (a throwaway session whose program wrote
// OSC 7501). The record has no `owner`, and `id` sits beside `record`.
const LIVE_CHANGED: &str = r#"{"type":"session_event","payload":{"session_id":"session:01a196ca-ea23-70c7-a0f5-bea252f4f6df","event":{"type":"block_event","payload":{"block_id":"block:01a196ca-ea23-7148-9fc0-352b07759d6f","creator_name":"com.superlogical.terminal","name":"program_status_changed","payload":{"id":"","record":{"state":"working","app":"x","effective_app":"x","msg":"hi","updated_at":"2026-11-01T22:07:52.056616+11:00"},"reason":"report"}}}}}"#;
const LIVE_CLOSED: &str = r#"{"type":"session_event","payload":{"session_id":"session:01a196ca-ea23-70c7-a0f5-bea252f4f6df","event":{"type":"block_closed","payload":{"block_id":"block:01a196ca-ea23-7148-9fc0-352b07759d6f"}}}}"#;

// A live removal, captured from Rex 2026-11-01 (a throwaway session whose program wrote
// `state=clear`). The removed ids sit in `ids`; `""` is the root record. The record of a live
// change event carries `owner.pid` when the program in the block wrote it.
const LIVE_REMOVED: &str = r#"{"type":"session_event","payload":{"session_id":"session:01a19704-54ee-7392-b814-ff045a554c13","event":{"type":"block_event","payload":{"block_id":"block:01a19704-54ee-7407-a42c-4ce8c9e5a775","creator_name":"com.superlogical.terminal","name":"program_status_removed","payload":{"ids":[""],"reason":"clear"}}}}}"#;

#[test]
fn a_live_removal_of_the_root_triggers_and_a_child_only_removal_does_not() {
    assert_eq!(
        parse_event_line(LIVE_REMOVED),
        Some(StatusEvent::Removed {
            block_id: "block:01a19704-54ee-7407-a42c-4ce8c9e5a775".to_string(),
        })
    );
    let child_only = LIVE_REMOVED.replace(r#""ids":[""]"#, r#""ids":["child"]"#);
    assert_eq!(parse_event_line(&child_only), None);
}

#[test]
fn a_live_line_and_a_flat_event_parse() {
    // The live record has no owner pid. The event is only a trigger for a call.
    assert_eq!(
        parse_event_line(LIVE_CHANGED),
        Some(StatusEvent::Changed {
            block_id: "block:01a196ca-ea23-7148-9fc0-352b07759d6f".to_string(),
        })
    );
    assert_eq!(
        parse_event_line(LIVE_CLOSED),
        Some(StatusEvent::BlockClosed {
            block_id: "block:01a196ca-ea23-7148-9fc0-352b07759d6f".to_string(),
        })
    );
    let with_id = LIVE_CHANGED.replace(r#""id":"""#, r#""id":"child""#);
    assert_eq!(parse_event_line(&with_id), None);
    let flat = json!({
        "type": "program_status_changed",
        "block_id": "block:f",
        "state": "blocked",
        "owner": {"pid": 9},
        "msg": "Allow?",
    })
    .to_string();
    assert_eq!(
        parse_event_line(&flat),
        Some(StatusEvent::Changed {
            block_id: "block:f".to_string(),
        })
    );
}

// -- the event streams ---------------------------------------------------------

/// A temp folder with a fake `rex` script. The script counts its runs, saves its pid, prints
/// fixed lines and, when `hold` is set, waits until a test kills it.
struct FakeProgram {
    dir: PathBuf,
    /// The open end of the fifo, so `cat` does not see the end of its input.
    writer: Mutex<Option<std::fs::File>>,
}

impl FakeProgram {
    fn new(name: &str, lines: &[String], hold: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("harold-rex-status-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut script = format!(
            "echo run >> '{runs}'\necho $$ > '{pid}'\ncat <<'END_OF_EVENTS'\n",
            runs = dir.join("runs").display(),
            pid = dir.join("pid").display(),
        );
        for line in lines {
            script.push_str(line);
            script.push('\n');
        }
        script.push_str("END_OF_EVENTS\n");
        if hold {
            script.push_str("exec sleep 30\n");
        }
        let path = dir.join("rex");
        write_fake_program(&path, Language::Shell, &script);
        Self {
            dir,
            writer: Mutex::new(None),
        }
    }

    /// A script that copies whatever a test sends with `feed`, as it comes.
    fn new_fifo(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("harold-rex-status-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("events");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let script = format!(
            "echo run >> '{runs}'\necho $$ > '{pid}'\nexec cat '{fifo}'\n",
            runs = dir.join("runs").display(),
            pid = dir.join("pid").display(),
            fifo = fifo.display(),
        );
        let path = dir.join("rex");
        write_fake_program(&path, Language::Shell, &script);
        Self {
            dir,
            writer: Mutex::new(None),
        }
    }

    /// Sends one event line to a script from `new_fifo`.
    fn feed(&self, line: &str) {
        use std::io::Write;
        let mut writer = self.writer.lock().unwrap();
        let file = writer.get_or_insert_with(|| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(self.dir.join("events"))
                .unwrap()
        });
        writeln!(file, "{line}").unwrap();
    }

    fn program(&self) -> Program {
        let path = self.dir.join("rex");
        Arc::new(move || Ok(path.clone()))
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.dir.join("runs")).map_or(0, |text| text.lines().count())
    }

    /// The pid of the script, once it has saved it.
    fn pid(&self) -> String {
        let mut pid = String::new();
        wait_until("the script saves its pid", || {
            pid = std::fs::read_to_string(self.dir.join("pid"))
                .map(|text| text.trim().to_string())
                .unwrap_or_default();
            pid.parse::<u32>().is_ok()
        });
        pid
    }
}

impl Drop for FakeProgram {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn slot_in(session: &str, block: &str) -> RexBlockSlot {
    RexBlockSlot {
        session_id: session.to_string(),
        session_label: session.to_string(),
        window: 1,
        pane: 1,
        block_id: block.to_string(),
        window_name: String::new(),
    }
}

fn changed_line(block: &str) -> String {
    event_line(
        "program_status_changed",
        &json!({"block_id": block, "id": "", "reason": "report", "record": {"state": "working"}}),
    )
}

fn status_with(
    api: impl Into<Arc<FakeRex>>,
    program: &FakeProgram,
    clock: Clock,
) -> (RexStatus, CurrentSteps) {
    let api: Arc<FakeRex> = api.into();
    let steps = CurrentSteps::default();
    let status = RexStatus::with_clock(api, program.program(), steps.clone(), clock);
    (status, steps)
}

fn now_clock() -> Clock {
    Arc::new(Instant::now)
}

/// A fake where each block answers `program_status` with the same reply every time.
fn fake_with_statuses(statuses: &[(&str, Value)]) -> FakeRex {
    FakeRex {
        sequences: statuses
            .iter()
            .map(|(block, value)| ((*block).to_string(), vec![Some(value.clone())]))
            .collect(),
        ..FakeRex::default()
    }
}

/// A fake where each block answers its replies in order. `None` is a failed call.
fn fake_with_sequences(sequences: &[(&str, Vec<Option<Value>>)]) -> Arc<FakeRex> {
    Arc::new(FakeRex {
        sequences: sequences
            .iter()
            .map(|(block, replies)| ((*block).to_string(), replies.clone()))
            .collect(),
        ..FakeRex::default()
    })
}

fn reply(state: &str, pid: u32, msg: &str) -> Value {
    json!({"records": [{"state": state, "owner": {"pid": pid}, "msg": msg}, {"id": "c", "state": "idle"}]})
}

fn no_root() -> Value {
    json!({"records": [{"id": "c", "state": "idle", "owner": {"pid": 1}}]})
}

fn record(state: RecordState, pid: u32, msg: Option<&str>) -> StatusRecord {
    StatusRecord {
        state,
        pid,
        msg: msg.map(str::to_string),
    }
}

fn working(pid: u32, msg: &str) -> Lookup {
    Lookup::Stream(Some(record(RecordState::Working, pid, Some(msg))))
}

#[test]
fn a_done_idle_and_error_record_write_no_step() {
    let program = FakeProgram::new("none", &[], true);
    let api = Arc::new(fake_with_statuses(&[
        ("block:d", reply("done", 1, "was busy")),
        ("block:i", reply("idle", 2, "was busy")),
        ("block:e", reply("error", 3, "was busy")),
        (
            "block:n",
            json!({"records": [{"state": "working", "owner": {"pid": 4}}]}),
        ),
        ("block:w", reply("working", 5, "busy")),
    ]));
    let (status, steps) = status_with(api, &program, now_clock());
    status.sync(&[
        slot_in("session:1", "block:d"),
        slot_in("session:1", "block:i"),
        slot_in("session:1", "block:e"),
        slot_in("session:1", "block:n"),
        slot_in("session:1", "block:w"),
    ]);
    wait_until("every block has a record", || {
        ["block:d", "block:i", "block:e", "block:n", "block:w"]
            .iter()
            .all(|block| {
                status.lookup(block) != Lookup::NoStream
                    && status.lookup(block) != Lookup::Stream(None)
            })
    });
    assert_eq!(steps.get("block:w", 5).as_deref(), Some("busy"));
    for (block, pid) in [
        ("block:d", 1),
        ("block:i", 2),
        ("block:e", 3),
        ("block:n", 4),
    ] {
        assert_eq!(steps.get(block, pid), None, "{block}");
    }
}

#[test]
fn a_stream_reads_the_first_record_of_each_block_then_a_call_replaces_it() {
    let program = FakeProgram::new("first", &[changed_line("block:a")], true);
    let api = fake_with_sequences(&[
        (
            "block:a",
            vec![
                Some(reply("working", 5, "first read")),
                Some(reply("done", 5, "x")),
            ],
        ),
        ("block:b", vec![Some(reply("blocked", 7, "Allow Bash?"))]),
    ]);
    let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[
        slot_in("session:1", "block:a"),
        slot_in("session:1", "block:b"),
    ]);
    wait_until("the call replaces the first read", || {
        status.lookup("block:a") == Lookup::Stream(Some(record(RecordState::Done, 5, None)))
    });
    assert_eq!(
        status.lookup("block:b"),
        Lookup::Stream(Some(record(RecordState::Blocked, 7, Some("Allow Bash?"))))
    );
    assert_eq!(steps.get("block:b", 7).as_deref(), Some("Allow Bash?"));
    assert_eq!(steps.get("block:a", 5), None);
    assert_eq!(status.lookup("block:other"), Lookup::NoStream);
    assert_eq!(program.runs(), 1);
    assert_eq!(api.status_calls("block:a"), 2);
    assert_eq!(api.status_calls("block:b"), 1);
}

#[test]
fn a_removed_event_triggers_a_call_and_the_answer_replaces_the_record() {
    // The event holds a record with another pid. Harold must not use it.
    let removed = event_line(
        "program_status_removed",
        &json!({"block_id": "block:a", "record": {"state": "done", "owner": {"pid": 99}}}),
    );
    let program = FakeProgram::new("removed", &[removed], true);
    let api = fake_with_sequences(&[(
        "block:a",
        vec![
            Some(reply("working", 5, "one")),
            Some(reply("working", 6, "two")),
        ],
    )]);
    let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the answer replaces the record", || {
        status.lookup("block:a") == working(6, "two")
    });
    assert_eq!(steps.get("block:a", 6).as_deref(), Some("two"));
    assert_eq!(steps.get("block:a", 5), None);
    assert_eq!(api.status_calls("block:a"), 2);
}

#[test]
fn an_answer_with_no_root_record_removes_the_record() {
    let program = FakeProgram::new("noroot", &[changed_line("block:a")], true);
    let api = fake_with_sequences(&[(
        "block:a",
        vec![Some(reply("working", 5, "one")), Some(no_root())],
    )]);
    let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the record is removed", || {
        api.status_calls("block:a") == 2 && status.lookup("block:a") == Lookup::Stream(None)
    });
    assert_eq!(steps.get("block:a", 5), None);
}

#[test]
fn a_burst_of_events_during_a_call_costs_one_more_call_not_one_each() {
    within_30s(
        "a_burst_of_events_during_a_call_costs_one_more_call_not_one_each",
        || {
            let mut lines: Vec<String> = (0..5).map(|_| changed_line("block:a")).collect();
            lines.push(event_line("block_closed", &json!({"block_id": "block:s"})));
            let program = FakeProgram::new_fifo("burst");
            let gate = Gate::new(
                "block:a",
                reply("working", 5, "held"),
                reply("working", 5, "later"),
            );
            let api = Arc::new(FakeRex {
                gate: Some(Arc::clone(&gate)),
                sequences: [("block:s".to_string(), vec![Some(reply("working", 9, "s"))])].into(),
                ..FakeRex::default()
            });
            let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
            let _open = gate.guard();
            // `block:s` is read first. `block:a` then holds the first call.
            status.sync(&[
                slot_in("session:1", "block:s"),
                slot_in("session:1", "block:a"),
            ]);
            // The call thread reads `block:s` first, then holds in `block:a`. So when the gate has
            // its call, the read of `block:s` is done. `block:s` then has no step only after
            // the reader handled the close, the last line.
            wait_until("the gate has its call", || gate.calls() == 1);
            // The lines come while the call is held, as the test name says.
            for line in &lines {
                program.feed(line);
            }
            wait_until("the reader read every line", || {
                steps.get("block:s", 9).is_none()
            });
            assert_eq!(gate.calls(), 1);
            gate.release();
            wait_until("the one more call answers", || {
                status.lookup("block:a") == working(5, "later")
            });
            assert_eq!(gate.calls(), 2);
            assert_eq!(api.status_calls("block:s"), 1);
            // A wrong third call would come just after the record. Check the count again.
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(gate.calls(), 2);
        },
    );
}

#[test]
fn block_closed_removes_the_block_with_no_call() {
    let closed = event_line("block_closed", &json!({"block_id": "block:a"}));
    let program = FakeProgram::new("closed", &[closed], true);
    let api = fake_with_sequences(&[("block:a", vec![Some(reply("working", 5, "one"))])]);
    let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the stream is ready", || {
        status.lookup("block:a") != Lookup::NoStream
    });
    // Even if the first read finished before the close, the block has no record now.
    wait_until("the block has no record", || {
        status.lookup("block:a") == Lookup::Stream(None)
    });
    assert_eq!(steps.get("block:a", 5), None);
    // The call thread skips a block that is closed already. So the one call happens only when
    // it starts before the reader handles the close, and the count is 0 or 1.
    assert!(api.status_calls("block:a") <= 1);
}

#[test]
fn a_close_between_the_check_and_the_store_does_not_bring_the_record_back() {
    within_30s(
        "a_close_between_the_check_and_the_store_does_not_bring_the_record_back",
        || {
            let lines = [
                event_line("block_closed", &json!({"block_id": "block:a"})),
                event_line("block_closed", &json!({"block_id": "block:s"})),
            ];
            let program = FakeProgram::new_fifo("closeheld");
            let gate = Gate::new(
                "block:a",
                reply("working", 5, "held"),
                reply("working", 5, "x"),
            );
            let api = Arc::new(FakeRex {
                gate: Some(Arc::clone(&gate)),
                sequences: [("block:s".to_string(), vec![Some(reply("working", 9, "s"))])].into(),
                ..FakeRex::default()
            });
            let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
            let _open = gate.guard();
            status.sync(&[
                slot_in("session:1", "block:s"),
                slot_in("session:1", "block:a"),
            ]);
            // The call thread reads `block:s` first, then holds in `block:a`. `block:s` has no
            // step only after the reader handled its close, which comes after the close of
            // `block:a`.
            wait_until("the gate has its call", || gate.calls() == 1);
            // The closes come while the call is held.
            for line in &lines {
                program.feed(line);
            }
            wait_until("the reader read both lines", || {
                steps.get("block:s", 9).is_none()
            });
            assert_eq!(gate.calls(), 1);
            gate.release();
            // The first read is ready only after the held call decided to store or not.
            wait_until("the held call is done", || {
                status.lookup("block:a") != Lookup::NoStream
            });
            assert_eq!(status.lookup("block:a"), Lookup::Stream(None));
            assert_eq!(steps.get("block:a", 5), None);
        },
    );
}

#[test]
fn a_failed_call_drops_the_record_and_logs_call_failed_once_until_a_call_works() {
    let (logs, _guard) = capture_logs();
    let program = FakeProgram::new_fifo("failed");
    let api = fake_with_sequences(&[(
        "block:a",
        vec![
            Some(reply("working", 5, "ok")),
            None,
            None,
            Some(reply("working", 5, "back")),
            None,
        ],
    )]);
    let (status, steps) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the first read", || {
        status.lookup("block:a") == working(5, "ok")
    });
    let failed = || {
        logs.with("agent status stream")
            .iter()
            .filter(|line| line.contains("event=\"call_failed\""))
            .count()
    };
    program.feed(&changed_line("block:a"));
    wait_until("the failed call drops the record", || {
        status.lookup("block:a") == Lookup::CallFailed
    });
    wait_until("the failure is logged", || failed() == 1);
    assert_eq!(steps.get("block:a", 5), None);
    let line = logs.with("call_failed").into_iter().next().unwrap();
    assert!(line.contains("session.id=\"session:1\""), "{line}");
    assert!(line.contains("reason=\"failed\""), "{line}");
    // A second failure logs nothing new.
    program.feed(&changed_line("block:a"));
    wait_until("the second call", || api.status_calls("block:a") == 3);
    // A call that works brings the record back.
    program.feed(&changed_line("block:a"));
    wait_until("the record returns", || {
        status.lookup("block:a") == working(5, "back")
    });
    assert_eq!(failed(), 1);
    // The next failure logs again.
    program.feed(&changed_line("block:a"));
    wait_until("the failure is logged again", || failed() == 2);
    assert_eq!(status.lookup("block:a"), Lookup::CallFailed);
}

fn is_failed(status: &RexStatus, block: &str) -> bool {
    status.lookup(block) == Lookup::CallFailed
}

#[test]
fn the_call_failed_mark_is_set_by_a_failed_call_and_cleared_by_a_working_one() {
    let program = FakeProgram::new_fifo("markset");
    let api = fake_with_sequences(&[(
        "block:a",
        vec![
            Some(reply("working", 5, "ok")),
            None,
            Some(reply("working", 5, "back")),
            None,
            Some(no_root()),
        ],
    )]);
    let (status, _) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the first read", || {
        status.lookup("block:a") == working(5, "ok")
    });
    program.feed(&changed_line("block:a"));
    wait_until("the mark is set", || is_failed(&status, "block:a"));
    program.feed(&changed_line("block:a"));
    wait_until("a working call clears it", || {
        status.lookup("block:a") == working(5, "back")
    });
    // A call that answers with no root record also counts as a call that works.
    program.feed(&changed_line("block:a"));
    wait_until("the mark is set again", || is_failed(&status, "block:a"));
    program.feed(&changed_line("block:a"));
    wait_until("an answer with no root clears it", || {
        status.lookup("block:a") == Lookup::Stream(None)
    });
}

#[test]
fn block_closed_clears_the_call_failed_mark() {
    let program = FakeProgram::new_fifo("markclosed");
    let api = fake_with_sequences(&[("block:a", vec![Some(reply("working", 5, "ok")), None])]);
    let (status, _) = status_with(Arc::clone(&api), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the first read", || {
        status.lookup("block:a") == working(5, "ok")
    });
    program.feed(&changed_line("block:a"));
    wait_until("the mark is set", || is_failed(&status, "block:a"));
    program.feed(&event_line("block_closed", &json!({"block_id": "block:a"})));
    wait_until("the close clears the mark", || {
        status.lookup("block:a") == Lookup::Stream(None)
    });
}

#[test]
fn dropping_the_stream_clears_the_call_failed_mark() {
    let program = FakeProgram::new("markdrop", &[], true);
    // The first stream's first call fails. The second stream's first call has no root record.
    let api = fake_with_sequences(&[("block:a", vec![None, Some(no_root())])]);
    let (status, _) = status_with(Arc::clone(&api), &program, now_clock());
    let slots = [slot_in("session:1", "block:a")];
    status.sync(&slots);
    wait_until("the mark is set", || is_failed(&status, "block:a"));
    status.sync(&[]);
    assert_eq!(status.lookup("block:a"), Lookup::NoStream);
    status.sync(&slots);
    wait_until("the new stream has no mark", || {
        status.lookup("block:a") == Lookup::Stream(None)
    });
}

#[test]
fn a_stale_generation_cannot_clear_the_call_failed_mark() {
    within_30s(
        "a_stale_generation_cannot_clear_the_call_failed_mark",
        || {
            let (logs, _guard) = capture_logs();
            let program = FakeProgram::new("markstaleclear", &[], true);
            // The old stream's held call works after its stream stopped. The new stream's call fails.
            let gate = Gate::with("block:a", Ok(no_root()), Err(RexError::Failed));
            let api = Arc::new(FakeRex {
                gate: Some(Arc::clone(&gate)),
                ..FakeRex::default()
            });
            let (status, _) = status_with(api, &program, now_clock());
            let _open = gate.guard();
            let slots = [slot_in("session:1", "block:a")];
            status.sync(&slots);
            wait_until("the old call is held", || gate.calls() == 1);
            status.sync(&[]);
            status.sync(&slots);
            wait_until("the new stream has the mark", || {
                is_failed(&status, "block:a")
            });
            gate.release();
            wait_until("the old reader ends", || ended_lines(&logs) == 1);
            assert!(is_failed(&status, "block:a"));
        },
    );
}

#[test]
fn a_bad_line_and_an_unknown_event_are_skipped() {
    let program = FakeProgram::new(
        "bad",
        &[
            "this is not json".to_string(),
            "{\"type\":".to_string(),
            event_line("client_connected", &json!({"block_id": "block:a"})),
            changed_line("block:a"),
        ],
        true,
    );
    let api = fake_with_sequences(&[(
        "block:a",
        vec![
            Some(reply("done", 5, "x")),
            Some(reply("working", 5, "after the bad lines")),
        ],
    )]);
    let (status, _) = status_with(api, &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the good line applies", || {
        status.lookup("block:a") == working(5, "after the bad lines")
    });
}

#[test]
fn an_over_long_line_is_discarded_and_the_stream_goes_on() {
    let program = FakeProgram::new(
        "toolong",
        &["x".repeat(2 * 1024 * 1024), changed_line("block:a")],
        true,
    );
    let api = fake_with_sequences(&[(
        "block:a",
        vec![
            Some(reply("done", 5, "x")),
            Some(reply("working", 5, "after the long line")),
        ],
    )]);
    let (status, _) = status_with(api, &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the line after the long one applies", || {
        status.lookup("block:a") == working(5, "after the long line")
    });
}

#[test]
fn a_stream_that_ends_drops_the_sessions_records_and_restarts_after_five_seconds() {
    let (logs, _guard) = capture_logs();
    let program = FakeProgram::new("ends", &[], false);
    let base = Instant::now();
    let offset = Arc::new(Mutex::new(Duration::ZERO));
    let clock: Clock = {
        let offset = Arc::clone(&offset);
        Arc::new(move || base + *offset.lock().unwrap())
    };
    let api = fake_with_statuses(&[("block:a", reply("working", 5, "step"))]);
    let (status, steps) = status_with(api, &program, clock);
    let slots = [slot_in("session:1", "block:a")];
    status.sync(&slots);
    wait_until("the stream ends", || ended_lines(&logs) == 1);
    wait_until("the records drop", || {
        status.lookup("block:a") == Lookup::NoStream
    });
    assert_eq!(steps.get("block:a", 5), None);
    let ended = logs
        .with("agent status stream")
        .into_iter()
        .find(|line| line.contains("event=\"ended\""))
        .unwrap();
    assert!(ended.contains("session.id=\"session:1\""), "{ended}");
    assert!(ended.contains("reason=\"exit\""), "{ended}");
    assert!(!ended.contains("step"), "{ended}");
    // Less than the gap later: no new child.
    *offset.lock().unwrap() = RESTART_GAP - Duration::from_millis(1);
    status.sync(&slots);
    assert_eq!(program.runs(), 1);
    // The gap has passed: a new child.
    *offset.lock().unwrap() = RESTART_GAP;
    status.sync(&slots);
    wait_until("the second run", || program.runs() == 2);
    let started = || {
        logs.with("agent status stream")
            .iter()
            .filter(|line| line.contains("event=\"started\""))
            .count()
    };
    // The line comes after the first read.
    wait_until("the second started line", || started() == 2);
}

fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn a_session_that_leaves_the_listing_has_its_child_killed() {
    let program = FakeProgram::new("leaves", &[], true);
    let api = fake_with_statuses(&[("block:a", reply("working", 5, "go"))]);
    let (status, steps) = status_with(api, &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the stream is ready", || {
        status.lookup("block:a") != Lookup::NoStream
    });
    let pid = program.pid();
    assert!(process_exists(&pid));
    status.sync(&[]);
    // The reader reaps the child on its own thread: wait for it, no zombie remains.
    wait_until("the child is reaped", || !process_exists(&pid));
    assert_eq!(status.lookup("block:a"), Lookup::NoStream);
    assert_eq!(steps.get("block:a", 5), None);
}

#[test]
fn stop_all_kills_and_reaps_every_child() {
    let program = FakeProgram::new("stopall", &[], true);
    let (status, _) = status_with(FakeRex::default(), &program, now_clock());
    status.sync(&[slot_in("session:1", "block:a")]);
    wait_until("the child runs", || program.runs() == 1);
    wait_until("the stream is ready", || {
        status.lookup("block:a") != Lookup::NoStream
    });
    let pid = program.pid();
    status.stop_all();
    assert!(!process_exists(&pid));
    assert_eq!(status.lookup("block:a"), Lookup::NoStream);
}

#[test]
fn an_absent_program_logs_rex_absent_and_leaves_no_stream() {
    let (logs, _guard) = capture_logs();
    let steps = CurrentSteps::default();
    let program: Program = Arc::new(|| Err(RexError::Absent));
    let status = RexStatus::new(Arc::new(FakeRex::default()), program, steps);
    status.sync(&[slot_in("session:1", "block:a")]);
    assert_eq!(status.lookup("block:a"), Lookup::NoStream);
    let lines = logs.with("agent status stream");
    assert_eq!(lines.len(), 1, "{lines:?}");
    for field in [
        "session.id=\"session:1\"",
        "event=\"ended\"",
        "reason=\"rex_absent\"",
        "records=0",
    ] {
        assert!(lines[0].contains(field), "{field} in {}", lines[0]);
    }
}

// -- log capture -----------------------------------------------------------------

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn with(&self, message: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|line| line.contains(message))
            .map(str::to_string)
            .collect()
    }
}

fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    crate::test_logs::keep_second_dispatcher();
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

fn ended_lines(logs: &Logs) -> usize {
    logs.with("agent status stream")
        .iter()
        .filter(|line| line.contains("event=\"ended\""))
        .count()
}

#[test]
fn a_stopped_reader_never_touches_a_newer_stream_of_the_same_session() {
    within_30s(
        "a_stopped_reader_never_touches_a_newer_stream_of_the_same_session",
        || {
            let (logs, _guard) = capture_logs();
            let program = FakeProgram::new("stale", &[], true);
            let gate = Gate::new(
                "block:a",
                reply("working", 1, "stale"),
                reply("working", 2, "fresh"),
            );
            let api = FakeRex {
                gate: Some(Arc::clone(&gate)),
                ..FakeRex::default()
            };
            let (status, steps) = status_with(api, &program, now_clock());
            let _open = gate.guard();
            let slots = [slot_in("session:1", "block:a")];
            status.sync(&slots);
            wait_until("the old reader is in its call", || gate.calls() == 1);
            // The session leaves and returns while the old call is held.
            status.sync(&[]);
            status.sync(&slots);
            wait_until("the new stream reads the fresh record", || {
                status.lookup("block:a")
                    == Lookup::Stream(Some(record(RecordState::Working, 2, Some("fresh"))))
            });
            gate.release();
            wait_until("the old reader ends", || ended_lines(&logs) == 1);
            assert_eq!(
                status.lookup("block:a"),
                Lookup::Stream(Some(record(RecordState::Working, 2, Some("fresh"))))
            );
            assert_eq!(steps.get("block:a", 2).as_deref(), Some("fresh"));
            assert_eq!(steps.get("block:a", 1), None);
        },
    );
}

#[test]
fn a_stopped_reader_reads_no_queued_line_and_makes_no_call() {
    within_30s(
        "a_stopped_reader_reads_no_queued_line_and_makes_no_call",
        || {
            let (logs, _guard) = capture_logs();
            // The queued line has no owner pid, so a read of it would call `program_status`.
            let program = FakeProgram::new("queued", &[LIVE_CHANGED.to_string()], true);
            let block = "block:01a196ca-ea23-7148-9fc0-352b07759d6f";
            let gate = Gate::new(
                block,
                reply("working", 1, "stale"),
                reply("working", 2, "later"),
            );
            let api = FakeRex {
                gate: Some(Arc::clone(&gate)),
                ..FakeRex::default()
            };
            let (status, steps) = status_with(api, &program, now_clock());
            let _open = gate.guard();
            status.sync(&[slot_in("session:1", block)]);
            wait_until("the reader is in its first call", || gate.calls() == 1);
            status.sync(&[]);
            gate.release();
            wait_until("the reader ends", || ended_lines(&logs) == 1);
            assert_eq!(gate.calls(), 1);
            assert_eq!(status.lookup(block), Lookup::NoStream);
            assert_eq!(steps.get(block, 1), None);
        },
    );
}

// -- the inventory starts the streams ---------------------------------------------

/// A status whose program is never found, so its streams end at once. Inventory tests that
/// look at other things use it.
fn inert_status() -> Arc<RexStatus> {
    let program: Program = Arc::new(|| Err(RexError::Absent));
    Arc::new(RexStatus::new(
        Arc::new(FakeRex::default()),
        program,
        CurrentSteps::default(),
    ))
}

/// A fake with one block in each of the sessions `session:1` and `session:2`. The fake
/// can list only the first `listed` sessions.
fn fake_of_two_sessions(listed: usize) -> FakeRex {
    let sessions = ["session:1", "session:2"];
    let blocks = ["block:a", "block:b"];
    let mut fake = fake(&[
        ("block:a", Ok(claude_process())),
        ("block:b", Ok(claude_process())),
    ]);
    fake.sessions = sessions[..listed]
        .iter()
        .map(|id| RexSession {
            id: (*id).to_string(),
            label: (*id).to_string(),
        })
        .collect();
    for (session, block) in sessions.iter().zip(blocks) {
        fake.views
            .insert((*session).to_string(), vec![vec![block.to_string()]]);
    }
    fake.sequences = blocks
        .iter()
        .map(|block| {
            (
                (*block).to_string(),
                vec![Some(reply("working", 5, "step"))],
            )
        })
        .collect();
    fake
}

#[test]
fn an_inventory_scan_starts_a_stream_for_each_listed_session() {
    let _directory = directory_test_lock();
    let program = FakeProgram::new("scan-starts", &[], true);
    let (status, _) = status_with(fake_of_two_sessions(2), &program, now_clock());
    let status = Arc::new(status);
    let inventory = RexAgentInventory::new(
        Arc::new(fake_of_two_sessions(2)),
        settings(),
        Arc::clone(&status),
        WindowNames::default(),
    );
    inventory.scan().unwrap();
    wait_until("one child for each session", || program.runs() == 2);
    wait_until("both streams are ready", || {
        status.lookup("block:a") != Lookup::NoStream && status.lookup("block:b") != Lookup::NoStream
    });
    // A second scan of the same sessions starts no second stream.
    let before = status.generations();
    assert_eq!(before.len(), 2);
    inventory.scan().unwrap();
    assert_eq!(status.generations(), before);
    status.stop_all();
}

#[test]
fn a_scan_after_a_stream_ended_and_the_gap_passed_changes_the_generation() {
    let _directory = directory_test_lock();
    let (logs, _guard) = capture_logs();
    let program = FakeProgram::new("scan-restarts", &[], false);
    let base = Instant::now();
    let offset = Arc::new(Mutex::new(Duration::ZERO));
    let clock: Clock = {
        let offset = Arc::clone(&offset);
        Arc::new(move || base + *offset.lock().unwrap())
    };
    let (status, _) = status_with(fake_of_two_sessions(1), &program, clock);
    let status = Arc::new(status);
    let inventory = RexAgentInventory::new(
        Arc::new(fake_of_two_sessions(1)),
        settings(),
        Arc::clone(&status),
        WindowNames::default(),
    );
    inventory.scan().unwrap();
    let before = status.generations();
    // `lookup` also gives `NoStream` before the first read, so wait for the end line.
    wait_until("the stream ends", || ended_lines(&logs) == 1);
    *offset.lock().unwrap() = RESTART_GAP;
    inventory.scan().unwrap();
    let after = status.generations();
    assert_ne!(after["session:1"], before["session:1"]);
    status.stop_all();
}

#[test]
fn a_sync_after_stop_all_starts_no_stream() {
    let program = FakeProgram::new("sync-after-stop", &[], true);
    let (status, _) = status_with(FakeRex::default(), &program, now_clock());
    status.stop_all();
    status.sync(&[slot_in("session:1", "block:a")]);
    assert_eq!(program.runs(), 0);
    assert_eq!(status.lookup("block:a"), Lookup::NoStream);
}

#[test]
fn a_session_that_leaves_the_listing_has_its_stream_stopped() {
    let _directory = directory_test_lock();
    let program = FakeProgram::new("scan-leaves", &[], true);
    let (status, _) = status_with(fake_of_two_sessions(2), &program, now_clock());
    let status = Arc::new(status);
    let both = RexAgentInventory::new(
        Arc::new(fake_of_two_sessions(2)),
        settings(),
        Arc::clone(&status),
        WindowNames::default(),
    );
    both.scan().unwrap();
    wait_until("both streams are ready", || {
        status.lookup("block:a") != Lookup::NoStream && status.lookup("block:b") != Lookup::NoStream
    });
    let one = RexAgentInventory::new(
        Arc::new(fake_of_two_sessions(1)),
        settings(),
        Arc::clone(&status),
        WindowNames::default(),
    );
    one.scan().unwrap();
    assert_eq!(status.lookup("block:b"), Lookup::NoStream);
    assert_ne!(status.lookup("block:a"), Lookup::NoStream);
    status.stop_all();
}

#[test]
fn a_tmux_setting_starts_no_stream() {
    let settings = crate::settings::settings_with("[agent_monitor]\nmultiplexer = \"tmux\"");
    let _inventory = inventory_with(&settings, WindowNames::default(), || {
        panic!("a tmux setting must not create the status streams")
    });
}

#[test]
fn a_rex_setting_gives_the_inventory_the_status() {
    let settings = crate::settings::settings_with("[agent_monitor]\nmultiplexer = \"rex\"");
    let asked = std::cell::Cell::new(false);
    let _inventory = inventory_with(&settings, WindowNames::default(), || {
        asked.set(true);
        inert_status()
    });
    assert!(asked.get());
}

// -- a pane id on another Rex host reaches no API call ---------------------------------------

#[test]
fn find_session_refuses_a_host_pane_id_without_listing() {
    let _directory = directory_test_lock();
    struct CountingRex(AtomicUsize, FakeRex);
    impl RexApi for CountingRex {
        fn sessions(&self) -> Result<Vec<RexSession>, RexError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.sessions()
        }
        fn session_view(&self, session: &str) -> Result<Vec<RexWindow>, RexError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.session_view(session)
        }
        fn process(&self, s: &str, b: &str) -> Result<RexProcess, RexError> {
            self.1.process(s, b)
        }
        fn start_time_ms(&self, pid: u32) -> Option<i64> {
            self.1.start_time_ms(pid)
        }
        fn block_call(&self, s: &str, b: &str, m: &str, a: &Value) -> Result<Value, RexError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.block_call(s, b, m, a)
        }
        fn paste(&self, s: &str, b: &str, t: &str) -> Result<(), RexError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.paste(s, b, t)
        }
        fn send_key(&self, s: &str, b: &str, k: &str) -> Result<(), RexError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.send_key(s, b, k)
        }
    }
    let api = CountingRex(
        AtomicUsize::new(0),
        fake(&[("block:abc", Ok(claude_process()))]),
    );

    for id in ["rex:omarchy/block:abc", "main:0.1", "%7"] {
        assert_eq!(find_session(&api, id), Ok(None), "{id}");
    }
    assert!(
        type_text_with(&api, "rex:omarchy/block:abc", "x")
            .unwrap_err()
            .contains("on_rex_host")
    );
    assert!(
        press_key_with(&api, "rex:omarchy/block:abc", "Enter")
            .unwrap_err()
            .contains("on_rex_host")
    );
    assert!(
        type_text_with(&api, "main:0.1", "x")
            .unwrap_err()
            .contains("invalid_pane_id")
    );
    assert!(
        press_key_with(&api, "main:0.1", "Enter")
            .unwrap_err()
            .contains("invalid_pane_id")
    );
    assert_eq!(api.0.load(Ordering::SeqCst), 0);
    assert!(api.1.calls.lock().unwrap().is_empty());
    assert!(api.1.pastes.lock().unwrap().is_empty());
    assert!(api.1.keys.lock().unwrap().is_empty());

    // A block id of this host works as before.
    type_text_with(&api, "block:abc", "hello").unwrap();
    press_key_with(&api, "block:abc", "Enter").unwrap();
    assert_eq!(api.1.pastes.lock().unwrap().len(), 1);
    assert_eq!(api.1.keys.lock().unwrap().len(), 1);
    assert!(api.0.load(Ordering::SeqCst) > 0);
}

#[test]
fn a_slow_call_holds_only_its_own_place_in_the_pool() {
    // Width 4. Item 0 waits until the 8 other items have finished. A pool lets the
    // 3 free places finish them while item 0 waits. Chunks of 4 never start items
    // 4 to 8 while item 0 holds its chunk, so the wait reaches its limit.
    let items: Vec<usize> = (0..9).collect();
    let finished = Mutex::new(0_usize);
    let changed = Condvar::new();
    let results = parallel_map(&items, 4, None, |item| {
        if *item == 0 {
            let guard = finished.lock().unwrap();
            let (_guard, wait) = changed
                .wait_timeout_while(guard, Duration::from_secs(10), |count| *count < 8)
                .unwrap();
            return !wait.timed_out();
        }
        *finished.lock().unwrap() += 1;
        changed.notify_all();
        true
    });
    assert_eq!(results.len(), 9);
    assert!(
        results[0],
        "item 0 waited 10 s: the 8 other items did not finish while it held its place"
    );
}

#[test]
fn a_pool_never_runs_more_than_width_calls() {
    let items: Vec<usize> = (0..16).collect();
    let running = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    parallel_map(&items, 8, None, |_| {
        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        running.fetch_sub(1, Ordering::SeqCst);
    });
    let peak = peak.load(Ordering::SeqCst);
    assert!(peak <= 8 && peak > 1, "peak was {peak}");
}

#[test]
#[should_panic(expected = "a Rex worker thread panicked")]
fn a_panic_in_a_pool_call_reaches_the_caller() {
    let items: Vec<usize> = (0..16).collect();
    parallel_map(&items, 4, None, |item| {
        if *item == 0 {
            panic!("a call failed");
        }
        *item
    });
}

#[test]
fn after_a_panic_the_other_workers_take_no_new_items() {
    let items: Vec<usize> = (0..16).collect();
    let started = AtomicUsize::new(0);
    let outcome = std::panic::catch_unwind(|| {
        parallel_map(&items, 4, None, |item| {
            started.fetch_add(1, Ordering::SeqCst);
            if *item == 0 {
                panic!("a call failed");
            }
            std::thread::sleep(Duration::from_millis(200));
        })
    });
    assert!(outcome.is_err());
    // Each of the 4 workers started one item; none took a second one.
    assert!(started.load(Ordering::SeqCst) <= 4);
}

#[test]
fn a_pool_keeps_the_order_of_the_items() {
    let items: Vec<u64> = (0..12).collect();
    let results = parallel_map(&items, 4, None, |item| {
        // Late items end first.
        std::thread::sleep(Duration::from_millis((12 - *item) * 5));
        *item * 10
    });
    let expected: Vec<u64> = items.iter().map(|item| item * 10).collect();
    assert_eq!(results, expected);
}

#[test]
fn a_pool_with_width_zero_or_no_items_returns_at_once() {
    let none: Vec<u32> = Vec::new();
    assert!(parallel_map(&none, 8, None, |item| *item).is_empty());
    assert!(parallel_map(&none, 0, None, |item| *item).is_empty());
    let items = vec![1u32, 2, 3];
    assert_eq!(
        parallel_map(&items, 0, None, |item| item + 1),
        vec![2, 3, 4]
    );
}

#[test]
fn a_stop_during_the_pool_ends_the_calls() {
    within_30s("a_stop_during_the_pool_ends_the_calls", || {
        let items: Vec<usize> = (0..16).collect();
        let signal = StopSignal::default();
        let stopper = signal.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stopper.request();
        });
        // Each call waits until its thread sees the stop.
        let results = parallel_map(&items, 4, Some(signal), |item| {
            while !stop_requested() {
                std::thread::sleep(Duration::from_millis(10));
            }
            *item
        });
        handle.join().unwrap();
        assert_eq!(results.len(), 16);
    });
}

// -- sessions and `ps` in the pool -------------------------------------------------------------

/// A gate for `count` calls. Each call that enters waits, up to 10 s, until all `count` calls
/// are inside. A serial pool never fills the gate, so the wait reaches its limit.
struct CallGate {
    count: usize,
    entered: Mutex<usize>,
    changed: Condvar,
}

impl CallGate {
    fn new(count: usize) -> Self {
        Self {
            count,
            entered: Mutex::new(0),
            changed: Condvar::new(),
        }
    }

    /// Returns false when the wait reached its limit.
    fn pass(&self) -> bool {
        let mut entered = self.entered.lock().unwrap();
        *entered += 1;
        self.changed.notify_all();
        let (_entered, wait) = self
            .changed
            .wait_timeout_while(entered, Duration::from_secs(10), |entered| {
                *entered < self.count
            })
            .unwrap();
        !wait.timed_out()
    }
}

/// A fake whose `session_view` and `start_time_ms` calls take time. A session can also fail.
/// With a gate, each of these calls also waits at the gate.
struct SlowRex {
    inner: FakeRex,
    view_delay: HashMap<String, Duration>,
    view_errors: HashMap<String, RexError>,
    ps_delay: Duration,
    ps_threads: Mutex<Vec<std::thread::ThreadId>>,
    gate: Option<Arc<CallGate>>,
    ps_gate: Option<Arc<CallGate>>,
    timed_out: AtomicBool,
}

impl SlowRex {
    fn new(inner: FakeRex) -> Self {
        Self {
            inner,
            view_delay: HashMap::new(),
            view_errors: HashMap::new(),
            ps_delay: Duration::ZERO,
            ps_threads: Mutex::new(Vec::new()),
            gate: None,
            ps_gate: None,
            timed_out: AtomicBool::new(false),
        }
    }

    fn pass_gate(&self, gate: &Option<Arc<CallGate>>) {
        if let Some(gate) = gate
            && !gate.pass()
        {
            self.timed_out.store(true, Ordering::SeqCst);
        }
    }
}

impl RexApi for SlowRex {
    fn sessions(&self) -> Result<Vec<RexSession>, RexError> {
        self.inner.sessions()
    }
    fn session_view(&self, session: &str) -> Result<Vec<RexWindow>, RexError> {
        if let Some(delay) = self.view_delay.get(session) {
            std::thread::sleep(*delay);
        }
        self.pass_gate(&self.gate);
        if let Some(error) = self.view_errors.get(session) {
            return Err(*error);
        }
        self.inner.session_view(session)
    }
    fn process(&self, s: &str, b: &str) -> Result<RexProcess, RexError> {
        self.inner.process(s, b)
    }
    fn start_time_ms(&self, pid: u32) -> Option<i64> {
        self.ps_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        std::thread::sleep(self.ps_delay);
        self.pass_gate(&self.ps_gate);
        self.inner.start_time_ms(pid)
    }
    fn block_call(&self, s: &str, b: &str, m: &str, a: &Value) -> Result<Value, RexError> {
        self.inner.block_call(s, b, m, a)
    }
    fn paste(&self, s: &str, b: &str, t: &str) -> Result<(), RexError> {
        self.inner.paste(s, b, t)
    }
    fn send_key(&self, s: &str, b: &str, k: &str) -> Result<(), RexError> {
        self.inner.send_key(s, b, k)
    }
}

/// A fake of `count` sessions with one block each. Block `n` runs an agent with pid `10 + n`.
fn slow_rex_of(count: usize) -> SlowRex {
    let names: Vec<(String, String)> = (0..count)
        .map(|n| (format!("session:{n}"), format!("block:{n}")))
        .collect();
    let processes: Vec<(String, RexProcess)> = names
        .iter()
        .enumerate()
        .map(|(n, (_, block))| {
            let pid = 10 + u32::try_from(n).unwrap();
            let process = process(
                1,
                Some(proc(pid, "claude", None)),
                vec![proc(1, "login", None)],
            );
            (block.clone(), process)
        })
        .collect();
    let refs: Vec<(&str, Result<RexProcess, RexError>)> = processes
        .iter()
        .map(|(block, process)| (block.as_str(), Ok(process.clone())))
        .collect();
    let mut inner = fake(&refs);
    inner.sessions = names
        .iter()
        .map(|(session, _)| RexSession {
            id: session.clone(),
            label: session.clone(),
        })
        .collect();
    for (session, block) in &names {
        inner
            .views
            .insert(session.clone(), vec![vec![block.clone()]]);
    }
    for n in 0..count {
        inner.starts.lock().unwrap().insert(
            10 + u32::try_from(n).unwrap(),
            1_000 + i64::try_from(n).unwrap(),
        );
    }
    SlowRex::new(inner)
}

#[test]
fn the_session_views_run_at_once() {
    let mut api = slow_rex_of(4);
    api.gate = Some(Arc::new(CallGate::new(4)));
    let (sessions, slots) = list_slots_counted(&api).unwrap();
    assert_eq!(sessions, 4);
    let blocks: Vec<&str> = slots.iter().map(|slot| slot.block_id.as_str()).collect();
    assert_eq!(blocks, ["block:0", "block:1", "block:2", "block:3"]);
    assert!(
        !api.timed_out.load(Ordering::SeqCst),
        "a session view waited 10 s: the 4 views did not run at once"
    );
}

#[test]
fn the_first_failing_session_gives_the_error() {
    let mut api = slow_rex_of(4);
    // Session 1 fails late with `Failed`. Session 3 fails at once with `Absent`.
    api.view_delay
        .insert("session:1".into(), Duration::from_millis(80));
    api.view_errors.insert("session:1".into(), RexError::Failed);
    api.view_errors.insert("session:3".into(), RexError::Absent);
    assert_eq!(list_slots_counted(&api), Err(RexError::Failed));
}

#[test]
fn new_agents_start_times_are_read_in_parallel() {
    let _directory = directory_test_lock();
    let mut api = slow_rex_of(4);
    api.ps_gate = Some(Arc::new(CallGate::new(4)));
    let api = Arc::new(api);
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let observed = inventory.scan().unwrap();
    let starts: Vec<i64> = observed
        .iter()
        .map(|pane| pane.incarnation.agent_started_at_ms)
        .collect();
    assert_eq!(starts, [1_000, 1_001, 1_002, 1_003]);
    assert!(
        !api.timed_out.load(Ordering::SeqCst),
        "a start-time read waited 10 s: the 4 reads did not run at once"
    );
    // The cache serves the next scan.
    let reads = api.ps_threads.lock().unwrap().len();
    inventory.scan().unwrap();
    assert_eq!(api.ps_threads.lock().unwrap().len(), reads);
}

#[test]
fn one_new_agent_reads_its_start_time_on_the_scan_thread() {
    let _directory = directory_test_lock();
    let api = Arc::new(slow_rex_of(1));
    let inventory = RexAgentInventory::new(
        api.clone(),
        settings(),
        inert_status(),
        WindowNames::default(),
    );
    let observed = inventory.scan().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].incarnation.agent_started_at_ms, 1_000);
    assert_eq!(
        *api.ps_threads.lock().unwrap(),
        vec![std::thread::current().id()]
    );
}

// -- the window name of each agent pane ----------------------------------------------------

fn shell_process() -> RexProcess {
    process(8, Some(proc(9, "zsh", None)), vec![proc(8, "login", None)])
}

#[test]
fn a_rex_scan_fills_the_names_of_agent_panes_only() {
    let _directory = directory_test_lock();
    let mut api = fake(&[
        ("block:a", Ok(claude_process())),
        ("block:b", Ok(shell_process())),
    ]);
    api.labels = vec!["pod".into()];
    let names = WindowNames::default();
    let inventory =
        RexAgentInventory::new(Arc::new(api), settings(), inert_status(), names.clone());

    let agents = inventory.scan().unwrap();

    assert_eq!(agents.len(), 1);
    assert_eq!(
        names.get(&agents[0].incarnation.pane_id),
        Some("pod".to_string())
    );
    assert_eq!(names.get("block:b"), None);
}

#[test]
fn a_failed_rex_scan_leaves_the_names() {
    let _directory = directory_test_lock();
    let mut api = fake(&[("block:a", Ok(claude_process()))]);
    api.labels = vec!["pod".into()];
    let names = WindowNames::default();
    let first = RexAgentInventory::new(Arc::new(api), settings(), inert_status(), names.clone());
    let pane_id = first.scan().unwrap()[0].incarnation.pane_id.clone();
    let version = *names.subscribe().borrow();

    let mut failing = fake(&[("block:a", Ok(claude_process()))]);
    failing.list_error = Some(RexError::Timeout);
    let second =
        RexAgentInventory::new(Arc::new(failing), settings(), inert_status(), names.clone());
    assert!(second.scan().is_err());

    assert_eq!(names.get(&pane_id), Some("pod".to_string()));
    assert_eq!(*names.subscribe().borrow(), version);
}

#[test]
fn a_gone_rex_agent_pane_leaves_the_names() {
    let _directory = directory_test_lock();
    let mut api = fake(&[("block:a", Ok(claude_process()))]);
    api.labels = vec!["pod".into()];
    let names = WindowNames::default();
    let first = RexAgentInventory::new(Arc::new(api), settings(), inert_status(), names.clone());
    let pane_id = first.scan().unwrap()[0].incarnation.pane_id.clone();
    assert_eq!(names.get(&pane_id), Some("pod".to_string()));

    // The agent exits: the block stays, and it holds only a shell.
    let gone = fake(&[("block:a", Ok(shell_process()))]);
    let second = RexAgentInventory::new(Arc::new(gone), settings(), inert_status(), names.clone());
    assert!(second.scan().unwrap().is_empty());

    assert_eq!(names.get(&pane_id), None);
}

#[test]
fn a_rename_of_a_rex_window_with_no_agent_raises_no_version() {
    let _directory = directory_test_lock();
    let names = WindowNames::default();
    let build = |shell_window: &str| {
        let mut api = fake(&[
            ("block:a", Ok(claude_process())),
            ("block:b", Ok(shell_process())),
        ]);
        api.windows = vec![vec!["block:a".into()], vec!["block:b".into()]];
        api.labels = vec!["pod".into(), shell_window.into()];
        RexAgentInventory::new(Arc::new(api), settings(), inert_status(), names.clone())
    };
    build("shell").scan().unwrap();
    let version = *names.subscribe().borrow();

    build("renamed").scan().unwrap();

    assert_eq!(*names.subscribe().borrow(), version);
}
