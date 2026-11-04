use std::collections::HashMap;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::screen::{CommandOutput, CommandRunner};
use super::screen_lock_linux::locked_with;

/// Answers by command line. A command with no answer fails to start.
#[derive(Default)]
struct FakeCommands {
    answers: HashMap<String, Option<(bool, String)>>,
    calls: Mutex<Vec<String>>,
}

impl FakeCommands {
    fn answer(mut self, line: &str, success: bool, stdout: &str) -> Self {
        self.answers
            .insert(line.to_owned(), Some((success, stdout.to_owned())));
        self
    }
    fn fail(mut self, line: &str) -> Self {
        self.answers.insert(line.to_owned(), None);
        self
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for FakeCommands {
    fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let line = format!("{program} {}", args.join(" "));
        self.calls.lock().unwrap().push(line.clone());
        match self.answers.get(&line) {
            Some(Some((success, stdout))) => Ok(CommandOutput {
                success: *success,
                stdout: stdout.clone().into_bytes(),
            }),
            _ => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

const PGREP: &str = "pgrep -u 1000 -x hyprlock";
const USER: &str = "loginctl show-user 1000 -p Display";
const SESSION: &str = "loginctl show-session 2 -p LockedHint";

fn base() -> FakeCommands {
    FakeCommands::default().answer("id -u", true, "1000\n")
}

#[derive(Clone, Default)]
struct LogText(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogText {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogText {
    type Writer = LogText;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `check` `times` times with a fresh warned flag and returns the results and the log.
fn run(runner: &FakeCommands, times: usize) -> (Vec<bool>, String) {
    let log = LogText::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let warned = AtomicBool::new(false);
    let results = (0..times).map(|_| locked_with(runner, &warned)).collect();
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    (results, text)
}

#[test]
fn hyprlock_running_is_locked() {
    let runner = base().answer(PGREP, true, "4242\n");
    let (results, log) = run(&runner, 1);
    assert_eq!(results, [true]);
    assert!(!log.contains("screen lock unknown"));
    assert!(
        !runner
            .calls()
            .iter()
            .any(|call| call.starts_with("loginctl"))
    );
}

#[test]
fn locked_hint_yes_is_locked() {
    let runner = base()
        .answer(PGREP, false, "")
        .answer(USER, true, "Display=2\n")
        .answer(SESSION, true, "LockedHint=yes\n");
    assert_eq!(run(&runner, 1).0, [true]);
}

#[test]
fn locked_hint_no_is_unlocked() {
    let runner = base()
        .answer(PGREP, false, "")
        .answer(USER, true, "Display=2\n")
        .answer(SESSION, true, "LockedHint=no\n");
    let (results, log) = run(&runner, 1);
    assert_eq!(results, [false]);
    assert!(!log.contains("screen lock unknown"));
}

#[test]
fn pgrep_failed_counts_unlocked_and_warns_once() {
    let runner = base().fail(PGREP).fail(USER);
    let (results, log) = run(&runner, 3);
    assert_eq!(results, [false, false, false]);
    assert_eq!(log.matches("screen lock unknown").count(), 1);
    assert!(log.contains("WARN"));
    assert!(log.contains("reason=\"pgrep_failed\"") || log.contains("reason=pgrep_failed"));
}

#[test]
fn no_display_session_warns_with_reason() {
    let runner = base()
        .answer(PGREP, false, "")
        .answer(USER, true, "Display=\n");
    let (results, log) = run(&runner, 1);
    assert_eq!(results, [false]);
    assert!(log.contains("no_display_session"));
}

#[test]
fn loginctl_failed_warns_with_reason() {
    let runner = base().answer(PGREP, false, "").fail(USER);
    let (results, log) = run(&runner, 1);
    assert_eq!(results, [false]);
    assert!(log.contains("loginctl_failed"));
}

#[test]
fn a_session_hint_that_fails_warns_with_loginctl_failed() {
    let runner = base()
        .answer(PGREP, false, "")
        .answer(USER, true, "Display=2\n")
        .answer(SESSION, false, "");
    let (results, log) = run(&runner, 1);
    assert_eq!(results, [false]);
    assert!(log.contains("loginctl_failed"));
}

#[test]
fn a_failed_pgrep_does_not_stop_loginctl_from_answering() {
    let runner = base().fail(PGREP).answer(USER, true, "Display=2\n").answer(
        SESSION,
        true,
        "LockedHint=yes\n",
    );
    let (results, log) = run(&runner, 1);
    assert_eq!(results, [true]);
    assert!(!log.contains("screen lock unknown"));
}
