//! The Cargo features this build has, and the guards that stop a build for the wrong target.

#[cfg(all(feature = "imessage", not(target_os = "macos")))]
compile_error!(
    "the imessage feature needs macOS. On Linux, build with --no-default-features --features linux-defaults."
);

#[cfg(all(feature = "calls", not(target_os = "macos")))]
compile_error!(
    "the calls feature needs macOS. On Linux, build with --no-default-features --features linux-defaults."
);

/// The features this build has, in the order `--build-features` prints them.
pub(crate) const FEATURES: &[&str] = &[
    #[cfg(feature = "imessage")]
    "imessage",
    #[cfg(feature = "calls")]
    "calls",
];

/// What `harold --build-features` prints: one feature per line, and nothing for a build with
/// no feature.
pub(crate) fn listing(features: &[&str]) -> String {
    features.iter().map(|feature| format!("{feature}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::{FEATURES, listing};

    #[test]
    fn listing_has_one_feature_per_line() {
        assert_eq!(listing(&["imessage", "calls"]), "imessage\ncalls\n");
    }

    #[test]
    fn listing_of_a_build_with_no_feature_is_empty() {
        assert_eq!(listing(&[]), "");
    }

    #[cfg(all(feature = "imessage", feature = "calls"))]
    #[test]
    fn mac_defaults_build_has_both_features() {
        assert_eq!(FEATURES, ["imessage", "calls"]);
    }

    #[cfg(not(any(feature = "imessage", feature = "calls")))]
    #[test]
    fn a_build_with_no_feature_lists_none() {
        assert!(FEATURES.is_empty());
    }
}
