use std::process::Command;

fn query(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub fn active_session() -> Option<String> {
    query(&["display-message", "-p", "#{session_name}"])
}

pub fn pane_session(pane_id: &str) -> Option<String> {
    query(&["display-message", "-t", pane_id, "-p", "#{session_name}"])
}

/// Returns the active pane of the session that `pane_id` belongs to.
pub fn active_pane_in_session(pane_id: &str) -> Option<String> {
    let session = pane_session(pane_id)?;
    query(&["display-message", "-t", &session, "-p", "#{pane_id}"])
}
