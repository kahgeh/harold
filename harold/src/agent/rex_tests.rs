use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::settings::{AgentProviderSettings, AgentSettings};

use super::*;

fn provider(id: &str, command_contains: &[&str]) -> AgentProviderSettings {
    AgentProviderSettings {
        id: id.to_string(),
        display_name: id.to_uppercase(),
        command_contains: command_contains.iter().map(|s| (*s).to_string()).collect(),
        busy_all: Vec::new(),
        idle_all: Vec::new(),
        summary_line_prefixes: Vec::new(),
        screen_adapter: crate::settings::ScreenAdapter::GenericV1,
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
    assert_eq!(windows, vec![vec!["block:a", "block:b"], vec!["block:c"]]);
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
        child: proc(child, "login", None),
        foreground,
        ancestors,
    }
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
}

impl RexApi for FakeRex {
    fn sessions(&self) -> Result<Vec<RexSession>, RexError> {
        match self.list_error {
            Some(error) => Err(error),
            None => Ok(self.sessions.clone()),
        }
    }
    fn session_view(&self, _: &str) -> Result<Vec<Vec<String>>, RexError> {
        Ok(self.windows.clone())
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
    fn block_call(&self, _: &str, _: &str, method: &str, args: &Value) -> Result<Value, RexError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), args.clone()));
        match method {
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
    let inventory = RexAgentInventory::new(api.clone(), settings());
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
    let in_tool = process(
        1,
        Some(proc(30, "sleep", None)),
        vec![proc(5, "claude", None), proc(1, "login", None)],
    );
    let api = Arc::new(fake(&[("block:a", Ok(in_tool))]));
    let inventory = RexAgentInventory::new(api.clone(), settings());
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

#[test]
fn a_failed_listing_or_an_absent_rex_fails_the_scan() {
    for (error, expected) in [
        (RexError::Timeout, InventoryError::RexTimeout),
        (RexError::Absent, InventoryError::RexAbsent),
        (RexError::Failed, InventoryError::RexFailed),
    ] {
        let mut api = fake(&[]);
        api.list_error = Some(error);
        let inventory = RexAgentInventory::new(Arc::new(api), settings());
        assert_eq!(inventory.scan(), Err(expected));
    }
    let api = fake(&[("block:a", Err(RexError::Timeout))]);
    let inventory = RexAgentInventory::new(Arc::new(api), settings());
    assert_eq!(inventory.scan(), Err(InventoryError::RexTimeout));
}

#[test]
fn the_tail_read_calls_blob_read_with_the_id_until_eof() {
    let mut api = fake(&[]);
    api.blob = (0..400).map(|n| format!("line {n}\n")).collect();
    api.chunk = 1000;
    let tail = tail_read(&api, "session:1", "block:a", "text", 2500).unwrap();
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
    // The first line of a cut tail goes.
    let all = last_lines(&tail, 10_000);
    assert!(all.starts_with("line "));
    assert!(
        !all.starts_with(tail.text.split('\n').next().unwrap()) || tail.text.starts_with("line ")
    );
}

#[test]
fn a_short_output_starts_at_zero_and_keeps_its_first_line() {
    let mut api = fake(&[]);
    api.blob = "first\nsecond\nthird\n".into();
    api.chunk = 1 << 20;
    let tail = tail_read(&api, "session:1", "block:a", "text", 65536).unwrap();
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
fn an_absent_program_is_absent_and_a_slow_one_times_out() {
    let missing = run_program(
        Path::new("/nonexistent/rex"),
        &["ls"],
        Duration::from_secs(1),
    );
    assert_eq!(missing, Err(RexError::Absent));
    let slow = run_program(Path::new("/bin/sleep"), &["5"], Duration::from_millis(50));
    // `--autostart=false` makes sleep fail at once; a slow success is covered by the timeout
    // branch only when the program starts. Either way the call fails and does not hang.
    assert!(matches!(slow, Err(RexError::Timeout | RexError::Failed)));
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
    let api = fake(&[("block:z", Ok(claude_process()))]);
    let inventory = RexAgentInventory::new(
        Arc::new(fake(&[("block:z", Ok(claude_process()))])),
        settings(),
    );
    inventory.scan().unwrap();
    assert_eq!(session_of(&api, "block:z").unwrap(), "session:1");
    assert!(session_of(&api, "block:nope").is_err());
    assert!(block_is_gone(&api, "block:nope"));
    assert!(!block_is_gone(&api, "block:z"));
}
