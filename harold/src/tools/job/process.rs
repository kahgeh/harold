//! The process port: whether a pid runs, and when its process started.

/// Reads a process by pid. The start time tells a process from a later one that gets
/// the same pid.
pub(crate) trait ProcessPort: Send + Sync {
    /// The start time of the process in seconds since the epoch, or `None` when no process
    /// has this pid now.
    fn start_time(&self, pid: u32) -> Option<i64>;
}

/// Reads the process table of this machine with `sysinfo`.
pub(crate) struct SystemProcesses;

impl ProcessPort for SystemProcesses {
    fn start_time(&self, pid: u32) -> Option<i64> {
        let pid = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            sysinfo::ProcessRefreshKind::nothing(),
        );
        let started = system.process(pid)?.start_time();
        i64::try_from(started).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_process_runs_and_a_huge_pid_does_not() {
        let port = SystemProcesses;
        assert!(port.start_time(std::process::id()).is_some());
        assert!(port.start_time(u32::MAX - 7).is_none());
    }
}
