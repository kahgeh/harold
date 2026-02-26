/// Allowlist env vars for AI CLI subprocesses.
pub(crate) fn ai_cli_env() -> Vec<(String, String)> {
    let allowed = [
        "PATH",
        "HOME",
        "ANTHROPIC_API_KEY",
        "TMPDIR",
        "LANG",
        "LC_ALL",
    ];
    std::env::vars()
        .filter(|(k, _)| allowed.contains(&k.as_str()))
        .collect()
}

/// Strip characters that break AppleScript string literals passed via `osascript -e`.
pub(crate) fn sanitise_for_applescript(text: &str) -> String {
    text.chars()
        .filter(|c| *c != '\n' && *c != '\r' && *c != '¬' && !c.is_control())
        .collect()
}
