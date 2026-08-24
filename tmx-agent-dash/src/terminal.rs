use std::fmt;
use std::io::{self, Stdout};
use std::panic::{self, PanicHookInfo};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;
use crate::text::sanitize_display;
use crate::ui;

const ERROR_LIMIT: usize = 512;

trait TerminalOperations: Send + Sync {
    fn enable_raw(&self) -> io::Result<()>;
    fn enter_alternate(&self) -> io::Result<()>;
    fn hide_cursor(&self) -> io::Result<()>;
    fn show_cursor(&self) -> io::Result<()>;
    fn leave_alternate(&self) -> io::Result<()>;
    fn disable_raw(&self) -> io::Result<()>;
}

struct CrosstermOperations;

impl TerminalOperations for CrosstermOperations {
    fn enable_raw(&self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate(&self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen).map(|_| ())
    }

    fn hide_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Hide).map(|_| ())
    }

    fn show_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Show).map(|_| ())
    }

    fn leave_alternate(&self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen).map(|_| ())
    }

    fn disable_raw(&self) -> io::Result<()> {
        disable_raw_mode()
    }
}

#[derive(Default)]
struct CleanupFlags {
    raw: bool,
    alternate: bool,
    cursor_hidden: bool,
}

struct CleanupState {
    operations: Arc<dyn TerminalOperations>,
    flags: Mutex<CleanupFlags>,
    panic_restore_error: Mutex<Option<String>>,
}

impl CleanupState {
    fn new(operations: Arc<dyn TerminalOperations>) -> Arc<Self> {
        Arc::new(Self {
            operations,
            flags: Mutex::new(CleanupFlags::default()),
            panic_restore_error: Mutex::new(None),
        })
    }

    fn mark_raw(&self) {
        self.flags.lock().expect("terminal cleanup lock").raw = true;
    }

    fn mark_alternate(&self) {
        self.flags.lock().expect("terminal cleanup lock").alternate = true;
    }

    fn mark_cursor_hidden(&self) {
        self.flags
            .lock()
            .expect("terminal cleanup lock")
            .cursor_hidden = true;
    }

    fn restore(&self) -> io::Result<()> {
        let panic_error = self
            .panic_restore_error
            .lock()
            .expect("terminal panic cleanup lock")
            .take();
        let current = self.restore_operations();
        if let Some(detail) = panic_error {
            return Err(io::Error::other(detail));
        }
        current
    }

    fn restore_operations(&self) -> io::Result<()> {
        let (show_cursor, leave_alternate, disable_raw) = {
            let mut flags = self.flags.lock().expect("terminal cleanup lock");
            let stages = (flags.cursor_hidden, flags.alternate, flags.raw);
            flags.cursor_hidden = false;
            flags.alternate = false;
            flags.raw = false;
            stages
        };

        let mut first_error = None;
        if show_cursor {
            record_first_error(&mut first_error, self.operations.show_cursor());
        }
        if leave_alternate {
            record_first_error(&mut first_error, self.operations.leave_alternate());
        }
        if disable_raw {
            record_first_error(&mut first_error, self.operations.disable_raw());
        }

        first_error.map_or(Ok(()), Err)
    }

    fn restore_after_panic(&self) {
        if let Err(error) = self.restore_operations() {
            *self
                .panic_restore_error
                .lock()
                .expect("terminal panic cleanup lock") = Some(error.to_string());
        }
    }
}

impl Drop for CleanupState {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn record_first_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

fn acquire_stages(cleanup: &Arc<CleanupState>) -> io::Result<()> {
    cleanup.operations.enable_raw()?;
    cleanup.mark_raw();

    if let Err(error) = cleanup.operations.enter_alternate() {
        return Err(initialization_error(error, cleanup.restore()));
    }
    cleanup.mark_alternate();

    if let Err(error) = cleanup.operations.hide_cursor() {
        return Err(initialization_error(error, cleanup.restore()));
    }
    cleanup.mark_cursor_hidden();
    Ok(())
}

fn initialization_error(primary: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => io::Error::new(
            primary.kind(),
            format!("{primary}; terminal rollback also failed: {cleanup}"),
        ),
    }
}

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

struct PanicHookScope {
    previous: Arc<Mutex<Option<PanicHook>>>,
    installed: bool,
}

impl PanicHookScope {
    fn install(cleanup: Arc<CleanupState>) -> Self {
        let owner = std::thread::current().id();
        let previous = Arc::new(Mutex::new(Some(panic::take_hook())));
        let hook_previous = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() == owner {
                cleanup.restore_after_panic();
            }
            if let Some(previous) = hook_previous.lock().expect("panic hook lock").as_ref() {
                previous(info);
            }
        }));
        Self {
            previous,
            installed: true,
        }
    }

    fn restore(&mut self) {
        if !self.installed {
            return;
        }
        self.installed = false;
        let installed_hook = panic::take_hook();
        if let Some(previous) = self.previous.lock().expect("panic hook lock").take() {
            panic::set_hook(previous);
        }
        drop(installed_hook);
    }
}

impl Drop for PanicHookScope {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Debug)]
pub struct TerminalError {
    operation: &'static str,
    detail: String,
}

impl TerminalError {
    pub(crate) fn new(operation: &'static str, source: io::Error) -> Self {
        Self {
            operation,
            detail: sanitize_display(&source.to_string(), ERROR_LIMIT),
        }
    }
}

impl fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "could not {}: {}", self.operation, self.detail)
    }
}

impl std::error::Error for TerminalError {}

pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    cleanup: Arc<CleanupState>,
    panic_hook: PanicHookScope,
    owner: ThreadId,
}

impl TerminalGuard {
    pub fn acquire() -> Result<Self, TerminalError> {
        let cleanup = CleanupState::new(Arc::new(CrosstermOperations));
        let panic_hook = PanicHookScope::install(Arc::clone(&cleanup));
        acquire_stages(&cleanup)
            .map_err(|error| TerminalError::new("initialize terminal modes", error))?;
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                return Err(TerminalError::new(
                    "initialize Ratatui terminal",
                    initialization_error(error, cleanup.restore()),
                ));
            }
        };
        Ok(Self {
            terminal,
            cleanup,
            panic_hook,
            owner: std::thread::current().id(),
        })
    }

    pub fn draw(&mut self, app: &App, now_ms: i64) -> Result<(), TerminalError> {
        self.terminal
            .draw(|frame| ui::render(frame, app, now_ms))
            .map(|_| ())
            .map_err(|error| TerminalError::new("render dashboard", error))
    }

    pub fn restore(&mut self) -> Result<(), TerminalError> {
        debug_assert_eq!(self.owner, std::thread::current().id());
        let result = self
            .cleanup
            .restore()
            .map_err(|error| TerminalError::new("restore terminal modes", error));
        self.panic_hook.restore();
        result
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(any(test, feature = "terminal-fault-harness"))]
mod fault_harness;

#[cfg(feature = "terminal-fault-harness")]
#[doc(hidden)]
pub use fault_harness::{FaultReport, FaultScenario, run_terminal_fault_harness};

#[cfg(test)]
mod tests {
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::fault_harness::{FaultScenario, exercise_fault_scenario};
    use super::{CleanupState, PanicHookScope, TerminalOperations, acquire_stages};

    static PANIC_HOOK_TEST: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct FakeOperations {
        calls: Mutex<Vec<&'static str>>,
        fail_on: Mutex<Vec<&'static str>>,
    }

    impl FakeOperations {
        fn failing(operation: &'static str) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                fail_on: Mutex::new(vec![operation]),
            })
        }

        fn failing_many(operations: impl IntoIterator<Item = &'static str>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                fail_on: Mutex::new(operations.into_iter().collect()),
            })
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn perform(&self, operation: &'static str) -> io::Result<()> {
            self.calls.lock().unwrap().push(operation);
            if self.fail_on.lock().unwrap().contains(&operation) {
                return Err(io::Error::other(format!("{operation} failed")));
            }
            Ok(())
        }
    }

    impl TerminalOperations for FakeOperations {
        fn enable_raw(&self) -> io::Result<()> {
            self.perform("enable_raw")
        }
        fn enter_alternate(&self) -> io::Result<()> {
            self.perform("enter_alternate")
        }
        fn hide_cursor(&self) -> io::Result<()> {
            self.perform("hide_cursor")
        }
        fn show_cursor(&self) -> io::Result<()> {
            self.perform("show_cursor")
        }
        fn leave_alternate(&self) -> io::Result<()> {
            self.perform("leave_alternate")
        }
        fn disable_raw(&self) -> io::Result<()> {
            self.perform("disable_raw")
        }
    }

    #[test]
    fn partial_initialization_rolls_back_only_successful_stages() {
        for (failure, expected) in [
            ("enable_raw", vec!["enable_raw"]),
            (
                "enter_alternate",
                vec!["enable_raw", "enter_alternate", "disable_raw"],
            ),
            (
                "hide_cursor",
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "leave_alternate",
                    "disable_raw",
                ],
            ),
        ] {
            let operations = FakeOperations::failing(failure);
            let cleanup = CleanupState::new(operations.clone());

            assert!(acquire_stages(&cleanup).is_err(), "{failure} must fail");
            assert_eq!(operations.calls(), expected, "rollback for {failure}");
        }
    }

    #[test]
    fn partial_initialization_reports_rollback_failure_after_continuing_cleanup() {
        let operations = FakeOperations::failing_many(["hide_cursor", "leave_alternate"]);
        let cleanup = CleanupState::new(operations.clone());

        let error = acquire_stages(&cleanup).unwrap_err();

        assert!(error.to_string().contains("hide_cursor failed"));
        assert!(error.to_string().contains("leave_alternate failed"));
        assert_eq!(
            operations.calls(),
            vec![
                "enable_raw",
                "enter_alternate",
                "hide_cursor",
                "leave_alternate",
                "disable_raw",
            ]
        );
    }

    #[test]
    fn restoration_attempts_each_stage_once_and_continues_after_error() {
        let operations = FakeOperations::failing("show_cursor");
        let cleanup = CleanupState::new(operations.clone());
        acquire_stages(&cleanup).unwrap();

        assert!(cleanup.restore().is_err());
        assert!(cleanup.restore().is_ok());
        drop(cleanup);

        assert_eq!(
            operations.calls(),
            vec![
                "enable_raw",
                "enter_alternate",
                "hide_cursor",
                "show_cursor",
                "leave_alternate",
                "disable_raw",
            ]
        );
    }

    #[test]
    fn panic_cleanup_then_explicit_cleanup_and_drop_are_exactly_once() {
        let operations = Arc::new(FakeOperations::default());
        let cleanup = CleanupState::new(operations.clone());
        acquire_stages(&cleanup).unwrap();

        cleanup.restore_after_panic();
        assert!(cleanup.restore().is_ok());
        drop(cleanup);

        assert_eq!(
            operations.calls(),
            vec![
                "enable_raw",
                "enter_alternate",
                "hide_cursor",
                "show_cursor",
                "leave_alternate",
                "disable_raw",
            ]
        );
    }

    #[test]
    fn panic_cleanup_failure_is_reported_by_explicit_restoration_without_retrying_stages() {
        let operations = FakeOperations::failing("show_cursor");
        let cleanup = CleanupState::new(operations.clone());
        acquire_stages(&cleanup).unwrap();

        cleanup.restore_after_panic();
        assert!(cleanup.restore().is_err());
        assert!(cleanup.restore().is_ok());

        assert_eq!(
            operations.calls(),
            vec![
                "enable_raw",
                "enter_alternate",
                "hide_cursor",
                "show_cursor",
                "leave_alternate",
                "disable_raw",
            ]
        );
    }

    #[test]
    fn every_explicit_restore_failure_continues_and_consumes_all_stages() {
        for failure in ["show_cursor", "leave_alternate", "disable_raw"] {
            let operations = FakeOperations::failing(failure);
            let cleanup = CleanupState::new(operations.clone());
            acquire_stages(&cleanup).unwrap();

            assert!(cleanup.restore().is_err(), "{failure} must be reported");
            assert!(cleanup.restore().is_ok(), "{failure} must be consumed");
            assert_eq!(
                operations.calls(),
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "show_cursor",
                    "leave_alternate",
                    "disable_raw",
                ],
                "later operations continue after {failure}"
            );
        }
    }

    #[test]
    fn terminal_construction_or_render_failure_restores_every_acquired_stage() {
        for failure_point in ["terminal construction", "render"] {
            let operations = Arc::new(FakeOperations::default());
            let cleanup = CleanupState::new(operations.clone());
            acquire_stages(&cleanup).unwrap();

            let operation_result: io::Result<()> = Err(io::Error::other(failure_point));
            assert!(operation_result.is_err());
            cleanup.restore().unwrap();

            assert_eq!(
                operations.calls(),
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "show_cursor",
                    "leave_alternate",
                    "disable_raw",
                ]
            );
        }
    }

    #[test]
    fn panic_hook_cleans_only_owner_thread_and_restores_previous_hook() {
        let _serial = PANIC_HOOK_TEST.lock().unwrap();
        let original = std::panic::take_hook();
        let previous_calls = Arc::new(AtomicUsize::new(0));
        let observed_previous = Arc::clone(&previous_calls);
        std::panic::set_hook(Box::new(move |_| {
            observed_previous.fetch_add(1, Ordering::SeqCst);
        }));

        let operations = Arc::new(FakeOperations::default());
        let cleanup = CleanupState::new(operations.clone());
        acquire_stages(&cleanup).unwrap();
        let mut scope = PanicHookScope::install(cleanup);

        let other_thread = std::thread::spawn(|| {
            let _ = catch_unwind(|| panic!("other thread"));
        });
        other_thread.join().unwrap();
        assert_eq!(
            operations.calls().len(),
            3,
            "other thread cannot clean owner terminal"
        );

        let _ = catch_unwind(AssertUnwindSafe(|| panic!("owner thread")));
        assert_eq!(operations.calls().len(), 6);
        scope.restore();

        let _ = catch_unwind(|| panic!("after restoration"));
        assert_eq!(previous_calls.load(Ordering::SeqCst), 3);
        assert_eq!(operations.calls().len(), 6);

        let installed_previous = std::panic::take_hook();
        drop(installed_previous);
        std::panic::set_hook(original);
    }

    #[test]
    fn fault_scenarios_exercise_the_expected_cleanup_boundaries() {
        let _serial = PANIC_HOOK_TEST.lock().unwrap();

        for (scenario, failures, expected_calls) in [
            (
                FaultScenario::PartialInitialization,
                vec!["hide_cursor"],
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "leave_alternate",
                    "disable_raw",
                ],
            ),
            (
                FaultScenario::RenderFailure,
                vec![],
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "show_cursor",
                    "leave_alternate",
                    "disable_raw",
                ],
            ),
            (
                FaultScenario::PanicCleanup,
                vec![],
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "show_cursor",
                    "leave_alternate",
                    "disable_raw",
                ],
            ),
            (
                FaultScenario::RestorationFailure,
                vec!["show_cursor"],
                vec![
                    "enable_raw",
                    "enter_alternate",
                    "hide_cursor",
                    "show_cursor",
                    "leave_alternate",
                    "disable_raw",
                ],
            ),
        ] {
            let operations = FakeOperations::failing_many(failures);
            let cleanup = CleanupState::new(operations.clone());

            exercise_fault_scenario(scenario, cleanup, || {
                Err(io::Error::other("injected render failure"))
            })
            .unwrap();

            assert_eq!(operations.calls(), expected_calls, "{scenario:?}");
        }
    }

    #[test]
    fn partial_initialization_fault_rejects_a_real_rollback_failure() {
        let _serial = PANIC_HOOK_TEST.lock().unwrap();
        let operations = FakeOperations::failing_many(["hide_cursor", "leave_alternate"]);
        let cleanup = CleanupState::new(operations);

        let error =
            exercise_fault_scenario(FaultScenario::PartialInitialization, cleanup, || Ok(()))
                .unwrap_err();

        assert!(error.to_string().contains("terminal rollback also failed"));
    }
}
