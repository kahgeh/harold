//! The chief's work records, kept in Turso files of their own. Task 6 adds
//! `start`; until then the items below have no caller outside the tests.

// used from Task 6
#[allow(dead_code)]
mod schema;
// used from Task 6
#[allow(dead_code)]
mod store;
#[cfg(test)]
mod test_support;

// used from Task 6
#[allow(dead_code)]
pub(crate) const NAME: &str = "work";
