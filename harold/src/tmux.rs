use std::process::Command;

fn query(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub fn pane_session(pane_id: &str) -> Option<String> {
    query(&["display-message", "-t", pane_id, "-p", "#{session_name}"])
}

/// Whether the session containing `pane_id` has an attached client.
pub fn is_session_attached(pane_id: &str) -> bool {
    let Some(session) = pane_session(pane_id) else {
        return false;
    };
    query(&["display-message", "-t", &session, "-p", "#{session_attached}"])
        .is_some_and(|v| v != "0")
}

/// Returns the active pane of the session that `pane_id` belongs to.
pub fn active_pane_in_session(pane_id: &str) -> Option<String> {
    let session = pane_session(pane_id)?;
    query(&["display-message", "-t", &session, "-p", "#{pane_id}"])
}
