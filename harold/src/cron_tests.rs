use super::*;

fn bits(mask: u64) -> Vec<u32> {
    (0..64).filter(|bit| mask & (1u64 << bit) != 0).collect()
}

fn minutes_of(text: &str) -> Vec<u32> {
    let cron = Cron::parse(&format!("{text} * * * *")).expect("parse");
    bits(cron.minutes)
}

fn cron(text: &str) -> Cron {
    Cron::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn lm(text: &str) -> LocalMinute {
    LocalMinute::parse(text).expect("local minute")
}

fn error_of(text: &str) -> CronError {
    Cron::parse(text).expect_err(text)
}

#[test]
fn each_field_form_parses() {
    assert_eq!(minutes_of("*"), (0..60).collect::<Vec<_>>());
    assert_eq!(minutes_of("5"), vec![5]);
    assert_eq!(minutes_of("1-5"), vec![1, 2, 3, 4, 5]);
    assert_eq!(minutes_of("1,3,5"), vec![1, 3, 5]);
    assert_eq!(minutes_of("*/15"), vec![0, 15, 30, 45]);
    assert_eq!(minutes_of("10-30/5"), vec![10, 15, 20, 25, 30]);
}

#[test]
fn month_and_weekday_names_in_any_case() {
    let cron = cron("0 0 * jan,DEC Mon,SUN");
    assert_eq!(bits(cron.months as u64), vec![1, 12]);
    assert_eq!(bits(cron.weekdays as u64), vec![0, 1]);
    let seven = self::cron("0 0 * * 7");
    let zero = self::cron("0 0 * * 0");
    assert_eq!(seven.weekdays, zero.weekdays);
    assert_eq!(bits(seven.weekdays as u64), vec![0]);
}

#[test]
fn day_or_weekday_rule() {
    // 2026-11-13 is a Friday. 2026-11-06 is a Friday. 2026-11-12 is a Thursday.
    let both = cron("0 0 13 * fri");
    assert!(both.matches(lm("2026-11-13 00:00")));
    assert!(both.matches(lm("2026-11-06 00:00")));
    assert!(both.matches(lm("2026-12-13 00:00"))); // a Sunday, the 13th
    assert!(!both.matches(lm("2026-11-12 00:00")));

    let friday = cron("0 0 * * fri");
    assert!(friday.matches(lm("2026-11-06 00:00")));
    assert!(!friday.matches(lm("2026-12-13 00:00")));

    let day = cron("0 0 13 * *");
    assert!(day.matches(lm("2026-12-13 00:00")));
    assert!(!day.matches(lm("2026-11-06 00:00")));
}

#[test]
fn each_error_sentence() {
    let cases = [
        (
            "* * * *",
            "a cron expression has 5 fields (minute hour day month weekday); this one has 4",
        ),
        ("75 * * * *", "minute 75 is out of range: use 0 to 59"),
        (
            "* * * * mun",
            "weekday \"mun\" is not a name: use sun, mon, tue, wed, thu, fri or sat",
        ),
        ("* 17-9 * * *", "hour range 17-9 runs backwards: write 9-17"),
        ("*/0 * * * *", "minute step 0 is too small: use 1 or more"),
        ("0 0 31 2 *", "this schedule never runs"),
    ];
    for (text, sentence) in cases {
        assert_eq!(error_of(text).to_string(), sentence, "{text}");
    }
    assert_eq!(error_of("* * * * mun").field(), "weekday");
    assert_eq!(error_of("75 * * * *").field(), "minute");
    assert_eq!(error_of("* 17-9 * * *").field(), "hour");
}

#[test]
fn review_focus_inputs() {
    assert_eq!(minutes_of("*/7"), vec![0, 7, 14, 21, 28, 35, 42, 49, 56]);
    assert_eq!(error_of("5/2 * * * *").field(), "minute");
    assert_eq!(error_of("1,,3 * * * *").field(), "minute");
    assert_eq!(error_of("1, * * * *").field(), "minute");
    assert!(Cron::parse("0 0 29 2 *").is_ok());
    let weekdays = cron("* * * * 5-7");
    assert_eq!(bits(weekdays.weekdays as u64), vec![0, 5, 6]);
    assert!(Cron::parse("*\t*  *   * \t*").is_ok());
    let long = "1".repeat(10_000);
    assert_eq!(
        error_of(&long).to_string(),
        "a cron expression is longer than 200 characters"
    );
    assert!(Cron::parse("é * * * *").is_err());
    assert!(Cron::parse("* * * * \u{1F600}").is_err());
    assert!(Cron::parse("").is_err());
    assert!(Cron::parse("99999999999999999999 * * * *").is_err());
}

#[test]
fn local_minute_basics() {
    let minute = lm("2026-12-31 23:59");
    assert_eq!(minute.to_string(), "2026-12-31 23:59");
    assert_eq!(minute.next().to_string(), "2027-01-01 00:00");
    assert_eq!(
        lm("2024-02-28 23:59").next().to_string(),
        "2024-02-29 00:00"
    );
    assert_eq!(lm("2026-11-01 10:00").weekday(), 0);
    assert_eq!(lm("2026-11-06 10:00").weekday(), 5);
    assert!(lm("2026-11-06 10:00") < lm("2026-11-06 10:01"));
    assert!(LocalMinute::parse("2026-13-01 00:00").is_none());
    assert!(LocalMinute::parse("2026-02-30 00:00").is_none());
    assert!(LocalMinute::parse("2026-02-03 24:00").is_none());
    assert!(LocalMinute::parse("2026-02-03T10:00").is_none());
    assert!(LocalMinute::parse("é").is_none());
}

#[test]
fn next_after_finds_the_next_match() {
    let nine = cron("0 9 * * *");
    assert_eq!(
        nine.next_after(lm("2026-11-03 09:00")),
        Some(lm("2026-11-04 09:00"))
    );
    assert_eq!(
        nine.next_after(lm("2026-11-03 08:59")),
        Some(lm("2026-11-03 09:00"))
    );
    let leap = cron("0 0 29 2 *");
    assert_eq!(
        leap.next_after(lm("2026-11-03 00:00")),
        Some(lm("2028-02-29 00:00"))
    );
}

#[test]
fn due_run_runs_the_newest_of_three_matches() {
    let every_ten = cron("*/10 * * * *");
    let due = due_run(
        &every_ten,
        Some(lm("2026-11-03 10:05")),
        lm("2026-11-03 10:30"),
        60,
    );
    assert_eq!(due.run, Some(lm("2026-11-03 10:30")));
    assert_eq!(due.missed, 2);
}

#[test]
fn due_run_outside_the_window_gives_no_run() {
    let every_ten = cron("*/10 * * * *");
    let due = due_run(
        &every_ten,
        Some(lm("2026-11-03 10:05")),
        lm("2026-11-03 10:39"),
        5,
    );
    assert_eq!(due.run, None);
    assert_eq!(due.missed, 3);
}

#[test]
fn due_run_without_a_checked_minute_gives_no_run() {
    let every_minute = cron("* * * * *");
    let none = due_run(&every_minute, None, lm("2026-11-03 10:30"), 60);
    assert_eq!(
        none,
        DueRun {
            run: None,
            missed: 0
        }
    );
    let later = due_run(
        &every_minute,
        Some(lm("2026-11-03 11:00")),
        lm("2026-11-03 10:30"),
        60,
    );
    assert_eq!(
        later,
        DueRun {
            run: None,
            missed: 0
        }
    );
    let same = due_run(
        &every_minute,
        Some(lm("2026-11-03 10:30")),
        lm("2026-11-03 10:30"),
        60,
    );
    assert_eq!(
        same,
        DueRun {
            run: None,
            missed: 0
        }
    );
}

#[test]
fn min_gap_of_common_schedules() {
    assert_eq!(cron("*/15 9-17 * * *").min_gap_minutes(), Some(15));
    assert_eq!(cron("* * * * *").min_gap_minutes(), Some(1));
    assert!(cron("0 9 * * *").min_gap_minutes().unwrap() >= 1440);
    assert_eq!(cron("59 23 * * *").min_gap_minutes(), Some(1440));
    assert_eq!(cron("0,59 0,23 * * *").min_gap_minutes(), Some(1));
}
