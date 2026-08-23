pub(crate) mod domain;
pub(crate) mod inventory;
mod summary;

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod inventory_tests;

#[cfg(test)]
#[path = "summary_tests.rs"]
mod summary_tests;
