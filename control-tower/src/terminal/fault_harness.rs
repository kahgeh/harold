use std::io;
#[cfg(feature = "terminal-fault-harness")]
use std::io::Write;
use std::panic;
use std::sync::Arc;
#[cfg(feature = "terminal-fault-harness")]
use std::sync::Mutex;
#[cfg(feature = "terminal-fault-harness")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "terminal-fault-harness")]
use crossterm::cursor::{Hide, Show};
#[cfg(feature = "terminal-fault-harness")]
use crossterm::execute;
#[cfg(feature = "terminal-fault-harness")]
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
#[cfg(feature = "terminal-fault-harness")]
use ratatui::Terminal;
#[cfg(feature = "terminal-fault-harness")]
use ratatui::backend::CrosstermBackend;

use super::{CleanupState, PanicHookScope, acquire_stages};
#[cfg(feature = "terminal-fault-harness")]
use super::{TerminalError, TerminalOperations};

/// Fault cases exercised by the standalone terminal cleanup harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultScenario {
    PartialInitialization,
    RenderFailure,
    PanicCleanup,
    RestorationFailure,
}

#[cfg(feature = "terminal-fault-harness")]
impl FaultScenario {
    pub const fn name(self) -> &'static str {
        match self {
            Self::PartialInitialization => "partial-init",
            Self::RenderFailure => "render-failure",
            Self::PanicCleanup => "panic-cleanup",
            Self::RestorationFailure => "restoration-failure",
        }
    }

    fn expected_calls(self) -> &'static [&'static str] {
        match self {
            Self::PartialInitialization => &[
                "enable_raw",
                "enter_alternate",
                "hide_cursor",
                "leave_alternate",
                "disable_raw",
            ],
            Self::RenderFailure | Self::PanicCleanup | Self::RestorationFailure => &[
                "enable_raw",
                "enter_alternate",
                "hide_cursor",
                "show_cursor",
                "leave_alternate",
                "disable_raw",
            ],
        }
    }
}

/// Successful result from a deterministic terminal fault exercise.
#[derive(Debug, Eq, PartialEq)]
#[cfg(feature = "terminal-fault-harness")]
pub struct FaultReport {
    scenario: FaultScenario,
    calls: Vec<&'static str>,
}

#[cfg(feature = "terminal-fault-harness")]
impl FaultReport {
    pub const fn scenario(&self) -> FaultScenario {
        self.scenario
    }

    pub fn calls(&self) -> &[&'static str] {
        &self.calls
    }
}

#[cfg(feature = "terminal-fault-harness")]
struct HarnessOperations {
    calls: Mutex<Vec<&'static str>>,
    real_errors: Mutex<Vec<String>>,
    scenario: FaultScenario,
}

#[cfg(feature = "terminal-fault-harness")]
impl HarnessOperations {
    fn new(scenario: FaultScenario) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            real_errors: Mutex::new(Vec::new()),
            scenario,
        })
    }

    fn call_before(
        &self,
        operation: &'static str,
        action: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.calls
            .lock()
            .expect("terminal harness call lock")
            .push(operation);
        if self.scenario == FaultScenario::PartialInitialization && operation == "hide_cursor" {
            return Err(io::Error::other("injected hide_cursor failure"));
        }
        self.record_real_error(operation, action())
    }

    fn call_after(
        &self,
        operation: &'static str,
        action: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.calls
            .lock()
            .expect("terminal harness call lock")
            .push(operation);
        self.record_real_error(operation, action())?;
        if self.scenario == FaultScenario::RestorationFailure && operation == "show_cursor" {
            return Err(io::Error::other("injected show_cursor restoration failure"));
        }
        Ok(())
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .expect("terminal harness call lock")
            .clone()
    }

    fn record_real_error(&self, operation: &'static str, result: io::Result<()>) -> io::Result<()> {
        if let Err(error) = &result {
            self.real_errors
                .lock()
                .expect("terminal harness error lock")
                .push(format!("{operation}: {error}"));
        }
        result
    }

    fn real_errors(&self) -> Vec<String> {
        self.real_errors
            .lock()
            .expect("terminal harness error lock")
            .clone()
    }
}

#[cfg(feature = "terminal-fault-harness")]
impl TerminalOperations for HarnessOperations {
    fn enable_raw(&self) -> io::Result<()> {
        self.call_before("enable_raw", enable_raw_mode)
    }

    fn enter_alternate(&self) -> io::Result<()> {
        self.call_before("enter_alternate", || {
            execute!(io::stdout(), EnterAlternateScreen).map(|_| ())
        })
    }

    fn hide_cursor(&self) -> io::Result<()> {
        self.call_before("hide_cursor", || execute!(io::stdout(), Hide).map(|_| ()))
    }

    fn show_cursor(&self) -> io::Result<()> {
        self.call_after("show_cursor", || execute!(io::stdout(), Show).map(|_| ()))
    }

    fn leave_alternate(&self) -> io::Result<()> {
        self.call_after("leave_alternate", || {
            execute!(io::stdout(), LeaveAlternateScreen).map(|_| ())
        })
    }

    fn disable_raw(&self) -> io::Result<()> {
        self.call_after("disable_raw", disable_raw_mode)
    }
}

#[cfg(feature = "terminal-fault-harness")]
struct FaultWriter<W> {
    inner: W,
    fail: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
}

#[cfg(feature = "terminal-fault-harness")]
impl<W: Write> Write for FaultWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.fail.load(Ordering::SeqCst) {
            self.observed.store(true, Ordering::SeqCst);
            return Err(io::Error::other("injected Ratatui backend write failure"));
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            self.observed.store(true, Ordering::SeqCst);
            return Err(io::Error::other("injected Ratatui backend flush failure"));
        }
        self.inner.flush()
    }
}

pub(super) fn exercise_fault_scenario(
    scenario: FaultScenario,
    cleanup: Arc<CleanupState>,
    render: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut panic_hook = PanicHookScope::install(Arc::clone(&cleanup));

    match scenario {
        FaultScenario::PartialInitialization => match acquire_stages(&cleanup) {
            Ok(()) => {
                cleanup.restore()?;
                return Err(io::Error::other(
                    "partial initialization unexpectedly succeeded",
                ));
            }
            Err(error) if error.to_string().contains("terminal rollback also failed") => {
                return Err(error);
            }
            Err(_) => {}
        },
        FaultScenario::RenderFailure => {
            acquire_stages(&cleanup)?;
            if render().is_ok() {
                cleanup.restore()?;
                return Err(io::Error::other("render unexpectedly succeeded"));
            }
            cleanup.restore()?;
        }
        FaultScenario::PanicCleanup => {
            acquire_stages(&cleanup)?;
            let panic = panic::catch_unwind(|| panic!("injected terminal harness panic"));
            if panic.is_ok() {
                cleanup.restore()?;
                return Err(io::Error::other("panic injection unexpectedly returned"));
            }
            cleanup.restore()?;
        }
        FaultScenario::RestorationFailure => {
            acquire_stages(&cleanup)?;
            if cleanup.restore().is_ok() {
                return Err(io::Error::other("restoration unexpectedly succeeded"));
            }
            cleanup.restore()?;
        }
    }

    panic_hook.restore();
    Ok(())
}

/// Runs one terminal fault against the real PTY and verifies its cleanup trace.
#[cfg(feature = "terminal-fault-harness")]
pub fn run_terminal_fault_harness(scenario: FaultScenario) -> Result<FaultReport, TerminalError> {
    let operations = HarnessOperations::new(scenario);
    let cleanup = CleanupState::new(operations.clone());
    let render_injection_observed = Arc::new(AtomicBool::new(false));
    let render_observed = Arc::clone(&render_injection_observed);
    let render_failure = || {
        let fail = Arc::new(AtomicBool::new(false));
        let writer = FaultWriter {
            inner: io::stdout(),
            fail: Arc::clone(&fail),
            observed: render_observed,
        };
        let mut terminal = Terminal::new(CrosstermBackend::new(writer))?;
        fail.store(true, Ordering::SeqCst);
        terminal
            .draw(|frame| {
                use ratatui::widgets::Paragraph;
                frame.render_widget(Paragraph::new("terminal fault harness"), frame.area());
            })
            .map(|_| ())
    };

    let exercise_result = exercise_fault_scenario(scenario, cleanup, render_failure);
    let real_errors = operations.real_errors();
    if !real_errors.is_empty() {
        return Err(TerminalError::new(
            "exercise terminal fault",
            io::Error::other(format!("real terminal operation failed: {real_errors:?}")),
        ));
    }
    exercise_result.map_err(|error| TerminalError::new("exercise terminal fault", error))?;

    if scenario == FaultScenario::RenderFailure && !render_injection_observed.load(Ordering::SeqCst)
    {
        return Err(TerminalError::new(
            "exercise terminal fault",
            io::Error::other("Ratatui backend failed before the injected write was observed"),
        ));
    }

    let calls = operations.calls();
    if calls != scenario.expected_calls() {
        return Err(TerminalError::new(
            "verify terminal cleanup trace",
            io::Error::other(format!(
                "expected {:?}, observed {calls:?}",
                scenario.expected_calls()
            )),
        ));
    }

    Ok(FaultReport { scenario, calls })
}

#[cfg(all(test, feature = "terminal-fault-harness"))]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::FaultWriter;

    #[test]
    fn fault_writer_records_that_the_injected_write_failure_was_observed() {
        let fail = Arc::new(AtomicBool::new(true));
        let observed = Arc::new(AtomicBool::new(false));
        let mut writer = FaultWriter {
            inner: Vec::new(),
            fail,
            observed: Arc::clone(&observed),
        };

        assert!(writer.write_all(b"render").is_err());
        assert!(observed.load(Ordering::SeqCst));
    }
}
