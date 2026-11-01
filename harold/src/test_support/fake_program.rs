//! The one way a test writes a fake program that it then runs.
//!
//! This file uses no other item of the crate, so that the unit tests take it as a module and
//! `harold/tests/install_checks.rs` takes the same file with `#[path]`.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// The language of a fake program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    Shell,
    Python,
}

/// The Python interpreter, found once. The fake program starts it directly.
fn python_executable() -> &'static str {
    static PYTHON: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PYTHON.get_or_init(|| {
        // On macOS /usr/bin/python3 is a developer-tool launcher. Invoke the
        // interpreter directly so a fake program cannot depend on that wrapper.
        let output = std::process::Command::new("/usr/bin/python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    })
}

/// Writes a fake program for the user only. The test starts it after the call, and the call
/// waits until the start can work.
///
/// The file has the first line of `language`, then a guard that ends the program at once when
/// its first argument is `--probe`, then `body`.
///
/// A start of a script fails with "Text file busy" (os error 26) while any process still holds
/// the file open for writing. Another test thread can fork while this file is open, and the
/// child keeps the write handle until it runs `exec`. So closing the file here is not enough.
/// A probe run that works shows that no such child is left, and the start by the code under
/// test then cannot meet the error.
pub(crate) fn write_fake_program(path: &Path, language: Language, body: &str) {
    let text = match language {
        Language::Shell => format!("#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\n{body}\n"),
        Language::Python => format!(
            "#!{}\nimport sys\nif sys.argv[1:2] == ['--probe']:\n    sys.exit(0)\n{body}\n",
            python_executable()
        ),
    };
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    probe_until_it_starts(path);
}

/// Runs the program with `--probe` until the start works. It retries only on "Text file busy",
/// every 5 ms, for at most 10 s. Any other error panics with its text.
fn probe_until_it_starts(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::process::Command::new(path).arg("--probe").status() {
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                assert!(
                    Instant::now() < deadline,
                    "the fake program stays busy: {error}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the probe run of the fake program failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    fn folder() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "harold-fake-program-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn the_helper_waits_for_a_child_that_holds_a_write_handle_and_then_starts_the_script() {
        let folder = folder();
        let path = folder.join("fake");
        // The child opens the file for writing, says so, and keeps it open for 300 ms.
        let mut holder = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "exec 3>>'{}'; echo ready; sleep 0.3",
                path.display()
            ))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");

        let started = Instant::now();
        write_fake_program(&path, Language::Shell, "echo ran");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the helper returned while the handle was open"
        );
        let output = Command::new(&path).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ran");
        holder.wait().unwrap();
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_python_fake_starts_on_the_first_try_with_no_handle_open() {
        let folder = folder();
        let path = folder.join("fake");
        let started = Instant::now();
        write_fake_program(&path, Language::Python, "print('ok')");
        assert!(started.elapsed() < Duration::from_secs(2));
        let output = Command::new(&path).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
        let probe = Command::new(&path).arg("--probe").output().unwrap();
        assert!(probe.status.success() && probe.stdout.is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    #[should_panic(expected = "No such file or directory")]
    fn an_error_other_than_busy_panics_with_its_text() {
        probe_until_it_starts(Path::new("/nonexistent-harold-fake/program"));
    }
}
