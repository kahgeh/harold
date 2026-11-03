//! Reads a spec's title from its markdown.

/// Returns the text after `<id>: ` in the first line that starts with `# `.
/// Returns an empty string when there is no such line or no `<id>: ` prefix.
pub(super) fn spec_title(id: &str, markdown: &str) -> String {
    let Some(heading) = markdown.lines().find_map(|line| line.strip_prefix("# ")) else {
        return String::new();
    };
    heading
        .strip_prefix(id)
        .and_then(|rest| rest.strip_prefix(": "))
        .map(|title| title.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "title_tests.rs"]
mod title_tests;
