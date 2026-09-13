use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{CommandOutput, CommandRunner, ScreenError};

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

impl<R: CommandRunner> PaneCapturePort for TmuxPaneCapture<R> {
    fn capture(
        &self,
        pane_id: &str,
        request: CaptureRequest,
    ) -> Result<StyledPaneCapture, ScreenError> {
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
        let output = self.runner.output("tmux", &args).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ScreenError::CaptureUnavailable
            } else {
                ScreenError::CaptureFailed
            }
        })?;
        if !output.success || output.stdout.len() > MAX_CAPTURE_BYTES {
            return Err(ScreenError::CaptureFailed);
        }
        let text = String::from_utf8(output.stdout).map_err(|_| ScreenError::CaptureFailed)?;
        Ok(StyledPaneCapture { text, request })
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
}
