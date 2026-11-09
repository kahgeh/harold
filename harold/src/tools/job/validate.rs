//! The checks of an `add` request. Each check gives the name of the field that failed.

/// The most characters in a job name.
pub(super) const NAME_MAX: usize = 48;
/// The most characters in the `what` line.
pub(super) const WHAT_MAX: usize = 120;
/// The longest expected run time: 7 days.
pub(super) const FOR_MAX_SECS: u32 = 7 * 24 * 3600;

/// A job name is 1 to 48 characters of `a-z`, `0-9` and `-`.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The most characters in a unit name.
pub(super) const UNIT_MAX: usize = 128;

/// A unit name is 1 to 128 characters of letters, digits and `: _ . @ -`, and does not
/// start with `-`, so `systemctl` never reads it as an option.
pub(super) fn valid_unit(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= UNIT_MAX
        && !unit.starts_with('-')
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'@' | b'-'))
}

/// Checks the fields of an add request. Returns the name of the first bad field. A job
/// has a pid or a unit, never both.
pub(super) fn check_add(
    name: &str,
    pid: u32,
    unit: &str,
    for_secs: u32,
    pane_id: &str,
    what: &str,
) -> Result<(), &'static str> {
    if !valid_name(name) {
        return Err("name");
    }
    if unit.is_empty() {
        if pid == 0 {
            return Err("pid");
        }
    } else {
        if pid != 0 {
            return Err("pid");
        }
        if !valid_unit(unit) {
            return Err("unit");
        }
    }
    if for_secs == 0 || for_secs > FOR_MAX_SECS {
        return Err("for_secs");
    }
    if pane_id.trim().is_empty() || pane_id.chars().count() > 120 {
        return Err("pane_id");
    }
    if what.chars().count() > WHAT_MAX || what.contains(['\n', '\r']) {
        return Err("what");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_1_to_48_characters_of_lowercase_digits_and_dash() {
        assert!(valid_name("calibration"));
        assert!(valid_name("macos-tests-2"));
        assert!(valid_name(&"a".repeat(48)));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(49)));
        assert!(!valid_name("Calibration"));
        assert!(!valid_name("has space"));
        assert!(!valid_name("under_score"));
    }

    #[test]
    fn a_unit_name_has_no_space_slash_or_leading_dash() {
        assert!(valid_unit("cal-1"));
        assert!(valid_unit("cal-1.service"));
        assert!(valid_unit("run-u123.service"));
        assert!(!valid_unit(""));
        assert!(!valid_unit("--now"));
        assert!(!valid_unit("a b"));
        assert!(!valid_unit("a/b"));
        assert!(!valid_unit(&"a".repeat(129)));
        assert_eq!(check_add("x", 0, "cal-1", 60, "%4", ""), Ok(()));
        assert_eq!(check_add("x", 5, "cal-1", 60, "%4", ""), Err("pid"));
        assert_eq!(check_add("x", 0, "-x", 60, "%4", ""), Err("unit"));
    }

    #[test]
    fn add_refuses_a_bad_pid_time_pane_and_what() {
        assert_eq!(check_add("x", 4121, "", 1200, "%4", ""), Ok(()));
        assert_eq!(check_add("X", 4121, "", 1200, "%4", ""), Err("name"));
        assert_eq!(check_add("x", 0, "", 1200, "%4", ""), Err("pid"));
        assert_eq!(check_add("x", 4121, "", 0, "%4", ""), Err("for_secs"));
        assert_eq!(
            check_add("x", 4121, "", FOR_MAX_SECS + 1, "%4", ""),
            Err("for_secs")
        );
        assert_eq!(check_add("x", 4121, "", 1200, " ", ""), Err("pane_id"));
        assert_eq!(check_add("x", 4121, "", 1200, "%4", "a\nb"), Err("what"));
        assert_eq!(
            check_add("x", 4121, "", 1200, "%4", &"w".repeat(121)),
            Err("what")
        );
    }
}
