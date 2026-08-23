pub(crate) mod domain;
pub(crate) mod inventory;
mod screen;
mod summary;

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod inventory_tests;

#[cfg(test)]
#[path = "screen_tests.rs"]
mod screen_tests;

#[cfg(test)]
#[path = "summary_tests.rs"]
mod summary_tests;
