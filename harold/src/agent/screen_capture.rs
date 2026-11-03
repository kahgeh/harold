use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::agent::pane_route::{PaneRoute, refuse_write, route_pane};
use crate::agent::runtime::valid_pane_id;
use crate::telemetry::{
    MultiplexerOperation, MultiplexerOutcome, ScreenPurpose, outcome_of_io,
    record_multiplexer_call, record_screen_read,
};

use super::{CommandOutput, CommandRunner, ScreenError, stop_requested};

pub(super) const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum CaptureScope {
    Visible,
    RecentHistory { lines: u16 },
}

#[derive(Clone, Copy)]
pub(super) struct CaptureRequest {
    pub(super) scope: CaptureScope,
    pub(super) preserve_styles: bool,
    /// Why the screen is read, as the `purpose` of the call.
    pub(super) purpose: ScreenPurpose,
}

// Raw terminal text deliberately has no Debug implementation.
pub(super) struct StyledPaneCapture {
    pub(super) text: String,
    pub(super) request: CaptureRequest,
}

pub(super) trait PaneCapturePort {
    fn capture(
        &self,
        pane_id: &str,
        request: CaptureRequest,
    ) -> Result<StyledPaneCapture, ScreenError>;
}

pub(super) struct TmuxPaneCapture<R> {
    pub(super) runner: R,
}

impl<R: CommandRunner> TmuxPaneCapture<R> {
    /// A failed read is `PaneDeparted` only when tmux answers that it no longer lists the pane.
    fn failure(&self, pane_id: &str) -> ScreenError {
        if !valid_pane_id(pane_id) {
            return ScreenError::CaptureFailed;
        }
        let filter = format!("#{{==:#{{pane_id}},{pane_id}}}");
        let args = ["list-panes", "-a", "-F", "#{pane_id}", "-f", &filter];
        let started = Instant::now();
        let result = self.runner.output("tmux", &args);
        let outcome = match &result {
            Ok(output) if output.success => MultiplexerOutcome::Ok,
            Ok(_) => MultiplexerOutcome::Failed,
            Err(error) => outcome_of_io(error),
        };
        record_multiplexer_call(MultiplexerOperation::List, outcome, started.elapsed());
        match result {
            Ok(output) if output.success && output.stdout.is_empty() => ScreenError::PaneDeparted,
            _ => ScreenError::CaptureFailed,
        }
    }
}

impl<R: CommandRunner> PaneCapturePort for TmuxPaneCapture<R> {
    fn capture(
        &self,
        pane_id: &str,
        request: CaptureRequest,
    ) -> Result<StyledPaneCapture, ScreenError> {
        // Only a tmux pane id reaches tmux. A pane id on another Rex host is refused.
        match route_pane(pane_id) {
            Ok(PaneRoute::Tmux) => {}
            Ok(PaneRoute::Rex) => return Err(ScreenError::CaptureFailed),
            Err(refusal) => {
                refuse_write(pane_id, "screen_capture", refusal);
                return Err(ScreenError::CaptureFailed);
            }
        }
        let start = match request.scope {
            CaptureScope::Visible => "0".to_string(),
            CaptureScope::RecentHistory { lines } if (1..=10_000).contains(&lines) => {
                format!("-{lines}")
            }
            CaptureScope::RecentHistory { .. } => return Err(ScreenError::CaptureFailed),
        };
        let mut args = vec!["capture-pane", "-p"];
        if request.preserve_styles {
            args.push("-e");
        }
        args.extend(["-S", &start, "-t", pane_id]);
        let started = Instant::now();
        let result = self.runner.output("tmux", &args);
        let outcome = match &result {
            Ok(output) if output.success => MultiplexerOutcome::Ok,
            Ok(_) => MultiplexerOutcome::Failed,
            Err(error) => outcome_of_io(error),
        };
        record_screen_read(request.purpose, outcome, started.elapsed());
        let output = result.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ScreenError::CaptureUnavailable
            } else {
                ScreenError::CaptureFailed
            }
        })?;
        if !output.success {
            return Err(self.failure(pane_id));
        }
        if output.stdout.len() > MAX_CAPTURE_BYTES {
            return Err(ScreenError::CaptureFailed);
        }
        let text = String::from_utf8(output.stdout).map_err(|_| ScreenError::CaptureFailed)?;
        Ok(StyledPaneCapture { text, request })
    }
}

/// Reads a Rex block's screen with a tail read (see `agent::rex`).
pub(super) struct RexPaneCapture {
    pub(super) api: std::sync::Arc<dyn crate::agent::rex::RexApi>,
}

impl PaneCapturePort for RexPaneCapture {
    fn capture(
        &self,
        pane_id: &str,
        request: CaptureRequest,
    ) -> Result<StyledPaneCapture, ScreenError> {
        use crate::agent::rex::{self, Keep, RexError};
        let keep = match request.scope {
            CaptureScope::Visible => Keep::Visible,
            CaptureScope::RecentHistory { lines } if (1..=10_000).contains(&lines) => {
                Keep::Lines(usize::from(lines))
            }
            CaptureScope::RecentHistory { .. } => return Err(ScreenError::CaptureFailed),
        };
        match rex::read_screen_text(
            &*self.api,
            pane_id,
            request.preserve_styles,
            keep,
            request.purpose,
        ) {
            Ok(text) if text.len() <= MAX_CAPTURE_BYTES => Ok(StyledPaneCapture { text, request }),
            Ok(_) => Err(ScreenError::CaptureFailed),
            Err(RexError::Absent) => Err(ScreenError::CaptureUnavailable),
            Err(_) if rex::block_is_gone(&*self.api, pane_id) => Err(ScreenError::PaneDeparted),
            Err(_) => Err(ScreenError::CaptureFailed),
        }
    }
}

/// Bound both time and memory without a reader thread that could outlive a timed-out child.
pub(super) fn bounded_output(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<CommandOutput> {
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    let mut stdout = Vec::new();
    let mut buffer = [0_u8; 8192];
    let result = loop {
        if started.elapsed() >= timeout {
            break Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        if stop_requested() {
            break Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        match reader.read(&mut buffer) {
            Ok(0) => match child.try_wait() {
                Ok(Some(status)) => {
                    break Ok(CommandOutput {
                        success: status.success(),
                        stdout,
                    });
                }
                Ok(None) => {}
                Err(error) => break Err(error),
            },
            Ok(length) => {
                if stdout.len().saturating_add(length) > max_bytes {
                    break Err(io::Error::from(io::ErrorKind::InvalidData));
                }
                stdout.extend_from_slice(&buffer[..length]);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => break Err(error),
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(
        not(feature = "timing-tests"),
        ignore = "timing: run with --features timing-tests"
    )]
    fn child_capture_drains_success_and_enforces_size_and_time_bounds() {
        let output =
            bounded_output("/bin/sh", &["-c", "printf safe"], Duration::from_secs(1), 8).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, b"safe");
        let error = bounded_output(
            "/bin/sh",
            &["-c", "printf 'too much data'"],
            Duration::from_secs(1),
            8,
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let started = Instant::now();
        let error = bounded_output("/bin/sleep", &["5"], Duration::from_millis(20), 8)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    struct CountingRunner(std::sync::atomic::AtomicUsize);

    impl CommandRunner for CountingRunner {
        fn output(&self, _: &str, _: &[&str]) -> io::Result<CommandOutput> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(CommandOutput {
                success: true,
                stdout: b"screen".to_vec(),
            })
        }
    }

    #[test]
    fn a_tmux_capture_of_a_host_pane_id_runs_no_command() {
        let capture = TmuxPaneCapture {
            runner: CountingRunner(Default::default()),
        };
        let request = CaptureRequest {
            scope: CaptureScope::Visible,
            preserve_styles: false,
            purpose: ScreenPurpose::State,
        };
        for id in ["rex:omarchy/block:cap1", "main:0.1", "block:abc"] {
            assert!(capture.capture(id, request).is_err(), "{id}");
        }
        assert_eq!(
            capture.runner.0.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(capture.capture("%7", request).unwrap().text, "screen");
        assert_eq!(
            capture.runner.0.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}
