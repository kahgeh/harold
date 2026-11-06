use std::fs::{File, OpenOptions};
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

fn handle(path: &std::path::Path) -> File {
    File::open(path).expect("open handle")
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

    let result = trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    );

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

    let result = trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    );

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

    trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    )
    .unwrap();
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

    let failure = trim(&directory.log(), &handle(&other), now(), without_lock).unwrap_err();

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

    let failure = trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    )
    .unwrap_err();

    assert_eq!(failure.reason, "write_failed");
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), log);
}

#[test]
fn a_missing_file_is_not_the_log() {
    let directory = TestDirectory::new();
    let failure = trim(&directory.log(), &handle(&directory.0), now(), without_lock).unwrap_err();
    assert_eq!(failure.reason, "not_the_log");
}

#[test]
fn the_read_to_set_length_step_runs_inside_the_hold() {
    let directory = TestDirectory::new();
    let log = format!("{}{}", colored(30, "old"), colored(2, "new"));
    std::fs::write(directory.log(), &log).unwrap();
    let path = directory.log();
    let mut seen = (String::new(), String::new());

    let result = trim(&directory.log(), &handle(&path), now(), |run| {
        seen.0 = std::fs::read_to_string(&path).unwrap();
        let outcome = run();
        seen.1 = std::fs::read_to_string(&path).unwrap();
        outcome
    });

    result.unwrap();
    assert_eq!(seen.0, log);
    assert_eq!(seen.1, colored(2, "new"));
}

/// Lines of about 1 KiB, all from the last hour, numbered from `first`.
fn big_lines(first: usize, count: usize) -> String {
    let filler = "x".repeat(1000);
    (first..first + count)
        .map(|number| format!("{} line {number:06} {filler}\n", stamp(1)))
        .collect()
}

#[test]
fn twelve_mb_of_recent_lines_trims_below_10_mb_keeping_newest() {
    let directory = TestDirectory::new();
    let log = big_lines(0, 12 * 1024);
    std::fs::write(directory.log(), &log).unwrap();

    let result = trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    )
    .unwrap();

    let after = std::fs::read_to_string(directory.log()).unwrap();
    assert!((after.len() as u64) < MAX_BYTES);
    assert!(log.ends_with(&after));
    assert!(after.starts_with(&stamp(1)));
    assert!(after.contains("line 012287 "));
    assert_eq!(
        result,
        Trimmed::Dropped {
            kept_bytes: after.len() as u64,
            dropped_bytes: (log.len() - after.len()) as u64,
        }
    );
}

#[test]
fn one_line_larger_than_the_limit_keeps_the_newest_line_not_an_empty_file() {
    let directory = TestDirectory::new();
    let huge = format!("{} {}\n", stamp(1), "y".repeat(MAX_BYTES as usize + 10));
    let log = format!("{}{huge}", big_lines(0, 3));
    std::fs::write(directory.log(), &log).unwrap();

    let result = trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    )
    .unwrap();

    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), huge);
    assert_eq!(
        result,
        Trimmed::Dropped {
            kept_bytes: huge.len() as u64,
            dropped_bytes: (log.len() - huge.len()) as u64,
        }
    );
}

#[test]
fn a_small_file_is_untouched() {
    let directory = TestDirectory::new();
    let log = big_lines(0, 10);
    std::fs::write(directory.log(), &log).unwrap();

    let result = trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    );

    assert_eq!(result.unwrap(), Trimmed::Nothing);
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), log);
}

/// One timed line of exactly `length` bytes, newline included.
fn line_of(hours: u64, length: usize) -> String {
    let head = format!("{} ", stamp(hours));
    format!("{head}{}\n", "z".repeat(length - head.len() - 1))
}

fn run(directory: &TestDirectory) -> Outcome {
    trim(
        &directory.log(),
        &handle(&directory.log()),
        now(),
        without_lock,
    )
}

#[test]
fn a_line_that_starts_exactly_max_bytes_from_the_end_is_dropped() {
    let directory = TestDirectory::new();
    let first = line_of(1, 2000);
    let second = line_of(1, 2000);
    let last = line_of(1, MAX_BYTES as usize - 2000);
    let log = format!("{}{first}{second}{last}", line_of(1, 2000));
    std::fs::write(directory.log(), &log).unwrap();

    let result = run(&directory).unwrap();

    let after = std::fs::read_to_string(directory.log()).unwrap();
    assert_eq!(after, last);
    assert!((after.len() as u64) < MAX_BYTES);
    assert_eq!(
        result,
        Trimmed::Dropped {
            kept_bytes: last.len() as u64,
            dropped_bytes: (log.len() - last.len()) as u64,
        }
    );
}

#[test]
fn a_file_of_exactly_max_bytes_is_untouched() {
    let directory = TestDirectory::new();
    let log = format!(
        "{}{}",
        line_of(1, 2000),
        line_of(1, MAX_BYTES as usize - 2000)
    );
    std::fs::write(directory.log(), &log).unwrap();

    assert_eq!(run(&directory).unwrap(), Trimmed::Nothing);
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), log);
}

#[test]
fn multi_byte_characters_near_the_cut_leave_valid_utf8() {
    let directory = TestDirectory::new();
    let filler = "\u{e9}".repeat(500);
    let log: String = (0..24_000)
        .map(|number| format!("{} line {number:06} {filler}\n", stamp(1)))
        .collect();
    std::fs::write(directory.log(), &log).unwrap();

    run(&directory).unwrap();

    let after = std::fs::read_to_string(directory.log()).expect("valid UTF-8");
    assert!((after.len() as u64) < MAX_BYTES);
    assert!(log.ends_with(&after));
    assert!(after.starts_with(&stamp(1)));
}

#[test]
fn a_file_of_untimed_lines_only_is_cut_at_a_line_start() {
    let directory = TestDirectory::new();
    let filler = "p".repeat(1000);
    let log: String = (0..12 * 1024)
        .map(|number| format!("plain {number:06} {filler}\n"))
        .collect();
    std::fs::write(directory.log(), &log).unwrap();

    run(&directory).unwrap();

    let after = std::fs::read_to_string(directory.log()).unwrap();
    assert!((after.len() as u64) < MAX_BYTES);
    assert!(after.starts_with("plain "));
    assert!(log.ends_with(&after));
    assert!(after.contains("plain 012287 "));
}

#[test]
fn the_24_hour_cut_and_the_size_cut_work_together() {
    let directory = TestDirectory::new();
    let old = "o".repeat(1000);
    let old_lines: String = (0..2048)
        .map(|number| format!("{} old {number:06} {old}\n", stamp(30)))
        .collect();
    let log = format!("{old_lines}{}", big_lines(0, 11 * 1024));
    std::fs::write(directory.log(), &log).unwrap();

    let result = run(&directory).unwrap();

    let after = std::fs::read_to_string(directory.log()).unwrap();
    assert!((after.len() as u64) < MAX_BYTES);
    assert!(!after.contains(" old "));
    assert!(after.contains("line 011263 "));
    let Trimmed::Dropped {
        kept_bytes,
        dropped_bytes,
    } = result
    else {
        panic!("expected a trim");
    };
    assert_eq!(kept_bytes, after.len() as u64);
    assert_eq!(kept_bytes + dropped_bytes, log.len() as u64);
}

#[test]
fn a_file_that_is_one_line_over_the_limit_is_left_whole() {
    let directory = TestDirectory::new();
    let huge = line_of(1, MAX_BYTES as usize + 100);
    std::fs::write(directory.log(), &huge).unwrap();

    assert_eq!(run(&directory).unwrap(), Trimmed::Nothing);
    assert_eq!(std::fs::read_to_string(directory.log()).unwrap(), huge);
}
