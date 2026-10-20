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

/// Strip line breaks, `¬` and other control characters, which would break the one-line
/// AppleScript string literal that `send_script` builds for `osascript -` to read on its
/// standard input.
pub(crate) fn sanitise_for_applescript(text: &str) -> String {
    text.chars()
        .filter(|c| *c != '\n' && *c != '\r' && *c != '¬' && !c.is_control())
        .collect()
}
