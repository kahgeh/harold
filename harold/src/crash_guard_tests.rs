use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-crashes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("crashes")
    }

    fn write(&self, contents: &str) {
        std::fs::write(self.file(), contents).expect("write crash file");
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.file()).expect("read crash file")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const T: u64 = 1_000_000;

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn runs(file: &Path, seconds: u64) -> CrashGuard {
    match begin(file, at(seconds)).expect("begin") {
        Start::Run(guard) => guard,
        Start::Refuse => panic!("start at {seconds} was refused"),
    }
}

fn refused(file: &Path, seconds: u64) -> bool {
    matches!(begin(file, at(seconds)).expect("begin"), Start::Refuse)
}

#[test]
fn the_crash_file_sits_beside_the_store() {
    assert_eq!(
        crash_file(Path::new("/bundle/data/store")),
        PathBuf::from("/bundle/data/crashes")
    );
}

#[test]
fn a_first_start_runs_and_marks_running() {
    let directory = TestDirectory::new();
    runs(&directory.file(), T);
    assert_eq!(directory.read(), "running\n");
}

#[test]
fn a_start_after_a_clean_exit_counts_nothing() {
    let directory = TestDirectory::new();
    directory.write("clean\n");
    runs(&directory.file(), T);
    assert_eq!(directory.read(), "running\n");
}

#[test]
fn two_starts_after_unclean_exits_within_ten_minutes_still_run() {
    let directory = TestDirectory::new();
    directory.write("running\n");
    runs(&directory.file(), T);
    runs(&directory.file(), T + 60);
    assert_eq!(
        directory.read(),
        format!("running\n{T}\n{}\n", T + 60),
        "both unclean starts are kept"
    );
}

#[test]
fn a_third_unclean_start_within_ten_minutes_gives_up() {
    let directory = TestDirectory::new();
    directory.write("running\n");
    runs(&directory.file(), T);
    runs(&directory.file(), T + 60);
    assert!(refused(&directory.file(), T + 120));
    assert_eq!(directory.read().lines().next(), Some("gave-up"));
    // It stays off at the next login too, however much later, until haroldctl start.
    assert!(refused(&directory.file(), T + 10_000));
    assert_eq!(directory.read().lines().next(), Some("gave-up"));
}

#[test]
fn unclean_starts_older_than_ten_minutes_do_not_count() {
    let directory = TestDirectory::new();
    directory.write(&format!("running\n{}\n{}\n", T - 700, T - 650));
    runs(&directory.file(), T);
    assert_eq!(directory.read(), format!("running\n{T}\n"));
}

#[test]
fn a_clean_shutdown_resets_the_count() {
    let directory = TestDirectory::new();
    directory.write("running\n");
    runs(&directory.file(), T);
    runs(&directory.file(), T + 60)
        .mark_clean()
        .expect("mark clean");
    assert_eq!(directory.read(), "clean\n");
    runs(&directory.file(), T + 120);
    // Two more unclean starts are allowed before the third gives up.
    runs(&directory.file(), T + 180);
    runs(&directory.file(), T + 240);
    assert!(refused(&directory.file(), T + 300));
}

#[test]
fn a_garbled_file_never_stops_harold() {
    for contents in [
        "",
        "bogus\nx\n",
        "running\nnot-a-number\nalso bad\n",
        "\u{0}\u{1}\u{2}",
    ] {
        let directory = TestDirectory::new();
        directory.write(contents);
        runs(&directory.file(), T);
    }
    // Garbage times are dropped; only this start counts.
    let directory = TestDirectory::new();
    directory.write("running\nnot-a-number\nalso bad\n");
    runs(&directory.file(), T);
    assert_eq!(directory.read(), format!("running\n{T}\n"));
}

#[test]
fn times_after_now_do_not_count() {
    // The clock moved back: unclean starts "in the future" are dropped, not counted.
    let directory = TestDirectory::new();
    directory.write(&format!("running\n{}\n{}\n", T + 5_000, T + 6_000));
    runs(&directory.file(), T);
    assert_eq!(directory.read(), format!("running\n{T}\n"));
}

#[test]
fn an_unwritable_folder_is_an_error_not_a_panic() {
    let directory = TestDirectory::new();
    let not_a_folder = directory.0.join("data");
    std::fs::write(&not_a_folder, "a file").expect("write file");
    assert!(begin(&not_a_folder.join("crashes"), at(T)).is_err());
}
