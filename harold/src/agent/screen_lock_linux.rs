//! The screen lock of a Linux machine, for the slave's `Lock` report.
//!
//! The order is: `pgrep -u <uid> -x hyprlock` (a hit means locked), then
//! `loginctl show-user <uid> -p Display` and `loginctl show-session <id> -p LockedHint`.
//! When no step answers, the machine counts unlocked and one warning says why. The parsing
//! and the decision are plain code over a `CommandRunner`, so they build and test on every
//! platform. A log line here holds no secret and no text of a turn.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::warn;

use super::screen::{CommandRunner, SystemCommandRunner};

/// Why no step answered. The label is the `reason` field of the warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unknown {
    PgrepFailed,
    LoginctlFailed,
    NoDisplaySession,
}

impl Unknown {
    fn label(self) -> &'static str {
        match self {
            Unknown::PgrepFailed => "pgrep_failed",
            Unknown::LoginctlFailed => "loginctl_failed",
            Unknown::NoDisplaySession => "no_display_session",
        }
    }
}

/// Set after the first `screen lock unknown` warning of this process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
static WARNED: AtomicBool = AtomicBool::new(false);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Whether the screen is locked, read with the real commands. The warning comes once per start.
pub(crate) fn system_screen_locked() -> bool {
    linux_screen_locked(&SystemCommandRunner)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Whether the screen is locked, read through `runner`. The warning comes once per start.
pub(crate) fn linux_screen_locked(runner: &dyn CommandRunner) -> bool {
    locked_with(runner, &WARNED)
}

/// The decision, with the "warned" flag given by the caller.
pub(super) fn locked_with(runner: &dyn CommandRunner, warned: &AtomicBool) -> bool {
    match read_lock(runner) {
        Ok(locked) => locked,
        Err(reason) => {
            if !warned.swap(true, Ordering::SeqCst) {
                warn!(reason = reason.label(), "screen lock unknown");
            }
            false
        }
    }
}

fn read_lock(runner: &dyn CommandRunner) -> Result<bool, Unknown> {
    let uid = read_uid(runner).ok_or(Unknown::PgrepFailed)?;
    let pgrep = runner.output("pgrep", &["-u", &uid, "-x", "hyprlock"]);
    // Exit 0 is a hit. Exit 1 is no match, which is an answer. A start error is no answer.
    let pgrep_failed = pgrep.is_err();
    if pgrep.as_ref().is_ok_and(|output| output.success) {
        return Ok(true);
    }
    match read_hint(runner, &uid) {
        Ok(locked) => Ok(locked),
        Err(_) if pgrep_failed => Err(Unknown::PgrepFailed),
        Err(reason) => Err(reason),
    }
}

fn read_uid(runner: &dyn CommandRunner) -> Option<String> {
    let output = runner.output("id", &["-u"]).ok().filter(|o| o.success)?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then_some(text)
}

fn read_hint(runner: &dyn CommandRunner, uid: &str) -> Result<bool, Unknown> {
    let display = property(runner, &["show-user", uid, "-p", "Display"], "Display")?;
    if display.is_empty() {
        return Err(Unknown::NoDisplaySession);
    }
    if !display.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(Unknown::LoginctlFailed);
    }
    let hint = property(
        runner,
        &["show-session", &display, "-p", "LockedHint"],
        "LockedHint",
    )?;
    match hint.as_str() {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(Unknown::LoginctlFailed),
    }
}

/// Runs `loginctl <args>` and returns the value of `Name=value` in its output.
fn property(runner: &dyn CommandRunner, args: &[&str], name: &str) -> Result<String, Unknown> {
    let output = runner
        .output("loginctl", args)
        .ok()
        .filter(|output| output.success)
        .ok_or(Unknown::LoginctlFailed)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{name}=");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(prefix.as_str()))
        .map(|value| value.trim().to_owned())
        .ok_or(Unknown::LoginctlFailed)
}
