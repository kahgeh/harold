pub(crate) mod domain;
pub(crate) mod inventory;
pub(crate) mod recap;
pub(crate) mod reducer;
pub(crate) mod rex;
pub(crate) mod runtime;
pub(crate) mod screen;
pub(crate) mod snapshot;
pub(crate) mod summary;

#[cfg(test)]
#[path = "recap_tests.rs"]
mod recap_tests;

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
#[path = "snapshot_tests.rs"]
mod snapshot_tests;

#[cfg(test)]
#[path = "summary_tests.rs"]
mod summary_tests;

pub(crate) mod pane_route;

#[cfg(test)]
#[path = "pane_route_tests.rs"]
mod pane_route_tests;

pub(crate) mod rex_hosts;

#[cfg(test)]
#[path = "rex_hosts_tests.rs"]
mod rex_hosts_tests;

pub(crate) mod rex_host_runner;

#[cfg(test)]
#[path = "rex_host_runner_tests.rs"]
mod rex_host_runner_tests;

pub(crate) mod waiting;

#[cfg(test)]
#[path = "waiting_tests.rs"]
mod waiting_tests;

pub(crate) mod focus;

#[cfg(test)]
#[path = "focus_tests.rs"]
mod focus_tests;

pub(crate) mod screen_lock_linux;

#[cfg(test)]
#[path = "screen_lock_linux_tests.rs"]
mod screen_lock_linux_tests;

pub(crate) mod slave_feed;

#[cfg(test)]
#[path = "slave_feed_tests.rs"]
mod slave_feed_tests;
