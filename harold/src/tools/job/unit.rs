//! The unit port: what `systemctl` says about a systemd unit. The real port runs
//! `systemctl`. Tests use a fake. A unit is on the machine that runs this Harold.

use std::time::Duration;

use crate::tools::BoxFuture;

/// The longest wait for one `systemctl` call.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The properties that the check reads.
const PROPERTIES: &str = "ActiveState,SubState,Result,ExecMainStatus,LoadState";

/// What `systemctl show` gave for a unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UnitState {
    pub active: String,
    pub sub: String,
    pub result: String,
    /// `ExecMainStatus`. `None` when the property is missing or not a number.
    pub exit_status: Option<i32>,
    pub load: String,
}

impl UnitState {
    /// The unit is loaded. A transient unit that ended cleanly is unloaded at once.
    pub(crate) fn exists(&self) -> bool {
        self.load != "not-found"
    }

    /// The job runs while the unit is `active` or `activating` and its sub state is not
    /// `exited`. A unit that is not found has ended.
    pub(crate) fn is_running(&self) -> bool {
        self.exists()
            && matches!(self.active.as_str(), "active" | "activating")
            && self.sub != "exited"
    }

    /// The exit code of an ended unit: `ExecMainStatus` while the unit still exists.
    pub(crate) fn exit_code(&self) -> Option<i32> {
        if self.exists() {
            self.exit_status
        } else {
            None
        }
    }
}

/// `systemctl` did not answer: it is missing, it timed out, or it failed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnitError;

pub(crate) trait UnitPort: Send + Sync {
    /// The state of the unit now.
    fn show<'a>(&'a self, unit: &'a str, user: bool)
    -> BoxFuture<'a, Result<UnitState, UnitError>>;
    /// Stops the unit. A failure is not told: the next check shows the state.
    fn stop<'a>(&'a self, unit: &'a str, user: bool) -> BoxFuture<'a, ()>;
    /// The `RemainAfterExit` property of the unit. `None` when it cannot be read.
    fn remain_after_exit<'a>(&'a self, unit: &'a str, user: bool) -> BoxFuture<'a, Option<bool>>;
}

/// Reads the output of `systemctl show -p ...`: one `Key=Value` for each line.
pub(super) fn parse_show(text: &str) -> UnitState {
    let mut state = UnitState::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key {
            "ActiveState" => state.active = value.into(),
            "SubState" => state.sub = value.into(),
            "Result" => state.result = value.into(),
            "LoadState" => state.load = value.into(),
            "ExecMainStatus" => state.exit_status = value.parse().ok(),
            _ => {}
        }
    }
    state
}

/// Runs the real `systemctl`.
pub(crate) struct SystemUnits;

async fn systemctl(user: bool, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    command.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(CALL_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    // `systemctl show` of a unit that is not found still succeeds, with `LoadState=not-found`.
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

impl UnitPort for SystemUnits {
    fn show<'a>(
        &'a self,
        unit: &'a str,
        user: bool,
    ) -> BoxFuture<'a, Result<UnitState, UnitError>> {
        Box::pin(async move {
            let text = systemctl(user, &["show", unit, "-p", PROPERTIES])
                .await
                .ok_or(UnitError)?;
            Ok(parse_show(&text))
        })
    }

    fn stop<'a>(&'a self, unit: &'a str, user: bool) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = systemctl(user, &["stop", unit]).await;
        })
    }

    fn remain_after_exit<'a>(&'a self, unit: &'a str, user: bool) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            let text = systemctl(user, &["show", unit, "-p", "RemainAfterExit"]).await?;
            let value = text.trim().strip_prefix("RemainAfterExit=")?;
            Some(value == "yes")
        })
    }
}

#[cfg(test)]
#[path = "unit_tests.rs"]
mod tests;
