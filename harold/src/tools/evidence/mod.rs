//! The evidence tool: after a release, it checks that the evidence queries of each merged
//! spec find their signal in ClickHouse, and writes the result in the pod folder.

mod files;
mod state;

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "files_tests.rs"]
mod files_tests;
