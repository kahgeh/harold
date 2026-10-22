use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-log-trim-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn log(&self) -> PathBuf {
        self.0.join("harold.log")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 2026-10-22T12:00:00Z.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_792_670_400)
}

/// The UTC time `hours` before [`now`], as Harold writes it.
fn stamp(hours: u64) -> String {
    let seconds = 1_792_670_400 - hours * 3600;
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.069324Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn colored(hours: u64, text: &str) -> String {
    format!(
        "\x1b[2m{}\x1b[0m \x1b[32m INFO\x1b[0m {text}\n",
        stamp(hours)
    )
}

fn without_lock(run: &mut dyn FnMut() -> Outcome) -> Outcome {
    run()
}

#[test]
fn keeps_only_the_lines_from_the_first_one_under_24_hours_old() {
    let directory = TestDirectory::new();
    let log = format!(
        "{}child says old\n{}child says middle\n{}child says new\n  and more\n",
        colored(30, "thirty"),
        colored(25, "twenty-five"),
        colored(23, "twenty-three"),
    );
    std::fs::write(directory.log(), &log).unwrap();

    let result = trim(&directory.log(), &directory.log(), now(), without_lock);

    let kept = format!(
        "{}child says new\n  and more\n",
        colored(23, "twenty-three")
    );
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), kept);
    assert_eq!(
        result.unwrap(),
        Trimmed::Dropped {
            kept_bytes: kept.len() as u64,
            dropped_bytes: (log.len() - kept.len()) as u64,
        }
    );
}

#[test]
fn a_file_with_nothing_old_is_not_written() {
    let directory = TestDirectory::new();
    let log = format!("{}stray\n{}", colored(23, "a"), colored(1, "b"));
    std::fs::write(directory.log(), &log).unwrap();
    let before = std::fs::metadata(directory.log())
        .unwrap()
        .modified()
        .unwrap();

    let result = trim(&directory.log(), &directory.log(), now(), without_lock);

    assert_eq!(result.unwrap(), Trimmed::Nothing);
    let after = std::fs::metadata(directory.log()).unwrap();
    assert_eq!(after.modified().unwrap(), before);
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), log);
}

#[test]
fn a_line_appended_after_the_trim_lands_at_the_new_end() {
    let directory = TestDirectory::new();
    let mut writer = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.log())
        .unwrap();
    writer.write_all(colored(30, "old").as_bytes()).unwrap();
    writer.write_all(colored(2, "new").as_bytes()).unwrap();

    trim(&directory.log(), &directory.log(), now(), without_lock).unwrap();
    writer.write_all(b"after\n").unwrap();

    let contents = std::fs::read(directory.log()).unwrap();
    assert!(!contents.contains(&0));
    assert_eq!(
        String::from_utf8(contents).unwrap(),
        format!("{}after\n", colored(2, "new"))
    );
}

#[test]
fn a_path_that_is_not_the_given_file_is_refused() {
    let directory = TestDirectory::new();
    let other = directory.0.join("other.log");
    let log = format!("{}{}", colored(30, "old"), colored(2, "new"));
    std::fs::write(directory.log(), &log).unwrap();
    std::fs::write(&other, "x\n").unwrap();

    let failure = trim(&directory.log(), &other, now(), without_lock).unwrap_err();

    assert_eq!(failure.reason, "not_the_log");
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), log);
}

#[test]
fn a_file_that_cannot_be_written_keeps_its_contents() {
    let directory = TestDirectory::new();
    let log = format!("{}{}", colored(30, "old"), colored(2, "new"));
    std::fs::write(directory.log(), &log).unwrap();
    let mut permissions = std::fs::metadata(directory.log()).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(directory.log(), permissions).unwrap();

    let failure = trim(&directory.log(), &directory.log(), now(), without_lock).unwrap_err();

    assert_eq!(failure.reason, "write_failed");
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), log);
}

#[test]
fn a_missing_file_is_not_the_log() {
    let directory = TestDirectory::new();
    let failure = trim(&directory.log(), &directory.log(), now(), without_lock).unwrap_err();
    assert_eq!(failure.reason, "not_the_log");
}
