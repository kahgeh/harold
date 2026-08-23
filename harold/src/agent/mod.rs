pub(crate) mod domain;
pub(crate) mod inventory;
pub(crate) mod reducer;
pub(crate) mod runtime;
pub(crate) mod screen;
pub(crate) mod summary;

#[cfg(test)]
#[path = "reducer_tests.rs"]
mod reducer_tests;

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod inventory_tests;

#[cfg(test)]
#[path = "screen_tests.rs"]
mod screen_tests;

#[cfg(test)]
#[path = "summary_tests.rs"]
mod summary_tests;
