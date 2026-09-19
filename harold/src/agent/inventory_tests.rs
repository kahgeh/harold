use crate::settings::{AgentProviderSettings, AgentSettings};

use super::domain::{AgentIncarnation, UNKNOWN_PROVIDER_ID};
use super::inventory::{
    InventoryError, ProcessInfo, ProviderResolution, TmuxPaneInfo, observe_pane,
    parse_process_table, parse_tmux_panes, resolve_provider,
};

fn provider(id: &str, command_contains: &[&str]) -> AgentProviderSettings {
    AgentProviderSettings {
        id: id.to_string(),
        display_name: id.to_uppercase(),
        command_contains: command_contains
            .iter()
            .map(|fragment| (*fragment).to_string())
            .collect(),
        busy_all: Vec::new(),
        idle_all: Vec::new(),
        summary_line_prefixes: Vec::new(),
        screen_adapter: crate::settings::ScreenAdapter::GenericV1,
        screen_history_lines: 2000,
    }
}

fn pane() -> TmuxPaneInfo {
    TmuxPaneInfo {
        pane_id: "%7".to_string(),
        session_name: "harold".to_string(),
        window_index: 2,
        pane_index: 1,
        pane_pid: 10,
        tty: "ttys007".to_string(),
        working_directory: "/work/harold".to_string(),
    }
}

fn process(pid: u32, ppid: u32, pgid: i32, command: &str) -> ProcessInfo {
    ProcessInfo {
        pid,
        ppid,
        pgid,
        tpgid: 50,
        tty: "ttys007".to_string(),
        started_at_ms: Some(i64::from(pid) * 1_000),
        command: command.to_string(),
    }
}

fn named_settings() -> AgentSettings {
    AgentSettings(vec![
        provider("codex", &["codex"]),
        provider("claude", &["claude"]),
    ])
}

#[test]
fn foreground_agent_beats_shallower_background_match() {
    let processes = [
        process(10, 1, 10, "/bin/zsh"),
        process(20, 10, 40, "codex helper"),
        process(30, 20, 50, "codex"),
    ];

    let observation = observe_pane(&pane(), &processes, &named_settings(), 99)
        .unwrap()
        .unwrap();

    assert_eq!(observation.incarnation.agent_pid, 30);
}

#[test]
fn shallowest_then_lowest_pid_breaks_non_foreground_ties() {
    let mut root = process(10, 1, 10, "/bin/zsh");
    root.tpgid = -1;
    let processes = [
        root,
        process(20, 10, 20, "codex"),
        process(19, 10, 19, "codex wrapper"),
        process(30, 19, 30, "codex"),
    ];

    let observation = observe_pane(&pane(), &processes, &named_settings(), 99)
        .unwrap()
        .unwrap();

    assert_eq!(observation.incarnation.agent_pid, 19);
}

#[test]
fn pane_root_and_wrapped_descendant_are_both_discoverable() {
    let root_agent = [process(10, 1, 10, "claude")];
    let root_observation = observe_pane(&pane(), &root_agent, &named_settings(), 99)
        .unwrap()
        .unwrap();
    assert_eq!(root_observation.incarnation.agent_pid, 10);
    assert_eq!(root_observation.incarnation.provider_id, "claude");

    let wrapped = [
        process(10, 1, 10, "/bin/zsh"),
        process(11, 10, 11, "node launcher"),
        process(12, 11, 12, "/opt/bin/claude --session"),
    ];
    let wrapped_observation = observe_pane(&pane(), &wrapped, &named_settings(), 99)
        .unwrap()
        .unwrap();
    assert_eq!(wrapped_observation.incarnation.agent_pid, 12);
}

#[test]
fn ambiguous_matches_remain_visible_as_unknown() {
    let ambiguous = AgentSettings(vec![
        provider("codex", &["agent"]),
        provider("future", &["future-agent"]),
    ]);
    let processes = [
        process(10, 1, 10, "/bin/zsh"),
        process(11, 10, 50, "future-agent"),
    ];
    let observation = observe_pane(&pane(), &processes, &ambiguous, 99)
        .unwrap()
        .unwrap();
    assert_eq!(observation.incarnation.provider_id, UNKNOWN_PROVIDER_ID);
}

#[test]
fn ambiguous_provider_resolution_is_typed_for_bounded_operator_reporting() {
    let settings = AgentSettings(vec![
        provider("codex", &["agent"]),
        provider("future", &["future-agent"]),
    ]);

    assert_eq!(
        resolve_provider(&settings, "future-agent"),
        ProviderResolution::Ambiguous { match_count: 2 }
    );
}

#[test]
fn selected_process_requires_an_os_start_time() {
    let mut agent = process(11, 10, 50, "codex");
    agent.started_at_ms = None;
    let processes = [process(10, 1, 10, "/bin/zsh"), agent];

    assert_eq!(
        observe_pane(&pane(), &processes, &named_settings(), 99),
        Err(InventoryError::MissingProcessStartTime)
    );
}

#[test]
fn process_or_provider_replacement_creates_a_new_incarnation() {
    let original = AgentIncarnation {
        pane_id: "%7".to_string(),
        pane_pid: 10,
        agent_pid: 20,
        agent_started_at_ms: 1_000,
        provider_id: "codex".to_string(),
    };

    let mut restarted = original.clone();
    restarted.agent_started_at_ms = 2_000;
    assert_ne!(original, restarted);

    let mut provider_replaced = original.clone();
    provider_replaced.provider_id = "claude".to_string();
    assert_ne!(original, provider_replaced);

    let mut pane_replaced = original.clone();
    pane_replaced.pane_pid = 99;
    assert_ne!(original, pane_replaced);
}

#[test]
fn process_cycles_do_not_become_pane_descendants() {
    let processes = [
        process(10, 1, 10, "/bin/zsh"),
        process(20, 21, 50, "codex"),
        process(21, 20, 50, "wrapper"),
    ];

    assert_eq!(
        observe_pane(&pane(), &processes, &named_settings(), 99).unwrap(),
        None
    );
}

#[test]
fn process_and_tmux_snapshots_parse_complete_identity_fields() {
    let processes =
        parse_process_table("20 10 50 50 ttys007 Sun Aug 23 10:11:12 2026 codex --profile local\n")
            .unwrap();
    assert_eq!(processes.len(), 1);
    assert_eq!(processes[0].pid, 20);
    assert_eq!(processes[0].ppid, 10);
    assert_eq!(processes[0].pgid, 50);
    assert_eq!(processes[0].tpgid, 50);
    assert_eq!(processes[0].tty, "ttys007");
    assert_eq!(processes[0].started_at_ms, Some(1_787_479_872_000));
    assert_eq!(processes[0].command, "codex --profile local");

    let panes = parse_tmux_panes("%7\tharold\t2\t1\t10\t/dev/ttys007\t/work/harold\n").unwrap();
    assert_eq!(panes, [pane()]);
}

#[test]
fn tab_delimited_tmux_rows_preserve_spaces_unicode_and_literal_escapes() {
    let rows = parse_tmux_panes("%7\tharold café\t2\t1\t10\t/dev/ttys007\t/work/a folder/\\037\n")
        .unwrap();
    assert_eq!(rows[0].session_name, "harold café");
    assert_eq!(rows[0].working_directory, r"/work/a folder/\037");
    assert_eq!(rows[0].tty, "ttys007");
}

#[test]
fn tab_delimited_tmux_rows_reject_ambiguous_or_invalid_fields() {
    for row in [
        "%7\tharold\t2\t1\t10\t/dev/ttys007",
        "%7\tharold\t2\t1\t10\t/dev/ttys007\t/work\textra",
        "%7\tharold\tbad\t1\t10\t/dev/ttys007\t/work",
        r"%7\037harold\0372\0371\03710\037/dev/ttys007\037/work",
    ] {
        assert_eq!(parse_tmux_panes(row), Err(InventoryError::MalformedOutput));
    }
}

#[test]
fn malformed_snapshots_are_errors_not_empty_successes() {
    assert_eq!(
        parse_process_table("not a process row\n"),
        Err(InventoryError::MalformedOutput)
    );
    assert_eq!(
        parse_tmux_panes("%7\tmissing-fields\n"),
        Err(InventoryError::MalformedOutput)
    );
}

#[test]
fn process_debug_never_exposes_the_raw_command() {
    let process = process(20, 10, 50, "codex --token TOP_SECRET_PROCESS_ARG");

    let debug = format!("{process:?}");

    assert!(debug.contains("pid: 20"));
    assert!(!debug.contains("TOP_SECRET_PROCESS_ARG"));
    assert!(!debug.contains("--token"));
}

#[test]
#[ignore = "requires tmux; uses an isolated socket and no user configuration"]
fn live_tmux_inventory_roundtrip_without_locale() {
    use std::process::Command;

    struct Server {
        socket: std::path::PathBuf,
        directory: std::path::PathBuf,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(&self.socket)
                .arg("kill-server")
                .output();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    let directory = std::env::temp_dir().join(format!(
        "ht-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    ));
    std::fs::create_dir(&directory).unwrap();
    let directory = directory.canonicalize().unwrap();
    let server = Server {
        socket: directory.join("socket"),
        directory,
    };
    let working_directory = server.directory.join(r"space café \037");
    std::fs::create_dir(&working_directory).unwrap();
    let created = Command::new("tmux")
        .args(["-u", "-S"])
        .arg(&server.socket)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "fixture café",
            "-c",
        ])
        .arg(&working_directory)
        .arg("sleep 60")
        .output()
        .unwrap();
    assert!(created.status.success(), "isolated tmux startup failed");
    for locale in [None, Some("C"), Some("en_US.UTF-8")] {
        let mut command = super::inventory::tmux_panes_command();
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("TMUX", format!("{},0,0", server.socket.display()));
        if let Some(locale) = locale {
            command.env("LANG", locale).env("LC_ALL", locale);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "isolated inventory command failed");
        let output = String::from_utf8(output.stdout).unwrap();
        let panes = parse_tmux_panes(&output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session_name, "fixture café");
        assert_eq!(
            panes[0].working_directory,
            working_directory.to_str().unwrap()
        );
        assert!(panes[0].pane_pid > 0);
    }
}
