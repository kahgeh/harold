//! Runs one program to its end within a time limit. Every program is run through a
//! `Runner`, so tests run none of them.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a program run through a `Runner` gets.
pub(crate) const MODEL_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs one program to its end. The daemon uses `SystemRunner`.
pub(crate) trait Runner {
    /// Whether `program` with `args` exited 0 within `timeout`.
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> bool;
}

pub(crate) struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> bool {
        run_with_timeout(program, args, timeout)
    }
}

/// Whether `program` exited 0 within `timeout`. One still running then is killed. Nothing
/// is read from or written to its standard streams.
pub(crate) fn run_with_timeout(program: &str, args: &[String], timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{MODEL_TIMEOUT, run_with_timeout};

    #[test]
    fn model_is_given_120_seconds() {
        assert_eq!(MODEL_TIMEOUT, Duration::from_secs(120));
    }

    #[test]
    fn run_with_timeout_kills_a_program_that_runs_too_long() {
        let started = std::time::Instant::now();
        assert!(!run_with_timeout(
            "sleep",
            &["5".to_string()],
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(run_with_timeout("true", &[], Duration::from_secs(5)));
        assert!(!run_with_timeout("false", &[], Duration::from_secs(5)));
        assert!(!run_with_timeout(
            "/no/such/program",
            &[],
            Duration::from_secs(5)
        ));
    }
}
