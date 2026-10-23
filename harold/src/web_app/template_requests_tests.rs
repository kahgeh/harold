//! Every Datastar request a web app template makes sends only the signals its handler reads,
//! so the reply being typed never rides along in a URL.

use std::path::{Path, PathBuf};

/// Each `@get(` and `@post(` call in `html`, from the `@` to its closing parenthesis.
fn requests(html: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for (start, _) in html
        .match_indices("@get(")
        .chain(html.match_indices("@post("))
    {
        let mut depth = 0usize;
        let mut end = None;
        for (offset, ch) in html[start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + offset + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        found.push(&html[start..end.expect("an @get or @post call that never closes")]);
    }
    found
}

/// The calls in `html` that send signals: every one but a form post.
fn signal_requests(html: &str) -> Vec<&str> {
    requests(html)
        .into_iter()
        .filter(|call| !call.contains("contentType: 'form'"))
        .collect()
}

/// Whether the call's `filterSignals` includes anything matching `reply`.
fn includes_reply(call: &str) -> bool {
    let include = call
        .split_once("include: /")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    include.contains("reply")
}

fn check(html: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for call in signal_requests(html) {
        if !call.contains("filterSignals: {include: /") {
            problems.push(format!("sends every signal: {call}"));
        } else if includes_reply(call) && !call.starts_with("@post('/replies'") {
            problems.push(format!("sends the reply draft: {call}"));
        }
    }
    problems
}

fn templates() -> Vec<(PathBuf, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/web_app");
    let mut found: Vec<_> = std::fs::read_dir(&dir)
        .expect("the web app templates folder")
        .map(|entry| entry.expect("a template entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .map(|path| {
            let html = std::fs::read_to_string(&path).expect("a readable template");
            (path, html)
        })
        .collect();
    found.sort();
    found
}

#[test]
fn every_template_request_sends_only_the_signals_its_handler_reads() {
    let templates = templates();
    assert!(
        templates
            .iter()
            .any(|(_, html)| !signal_requests(html).is_empty()),
        "found no @get or @post to check"
    );
    for (path, html) in &templates {
        let problems = check(html);
        assert!(problems.is_empty(), "{}: {problems:#?}", path.display());
    }
}

#[test]
fn the_check_fails_for_a_request_without_a_filter_or_one_that_includes_the_reply() {
    let filtered = r#"data-init="@get('/updates', {filterSignals: {include: /^oldest$/}})""#;
    assert!(check(filtered).is_empty());
    let reply_post =
        r#"data-on:submit="@post('/replies', {filterSignals: {include: /^(reply|photo)$/}})""#;
    assert!(check(reply_post).is_empty());
    let form = r#"@post('/todos/done', {contentType: 'form'})"#;
    assert!(check(form).is_empty());

    let unfiltered = r#"data-on-intersect="@get('/thread/older')""#;
    assert_eq!(check(unfiltered).len(), 1);
    let no_include = r#"@get('/updates', {retry: 'error'})"#;
    assert_eq!(check(no_include).len(), 1);
    let leaks = r#"@get('/updates', {filterSignals: {include: /^(oldest|reply)$/}})"#;
    assert_eq!(check(leaks).len(), 1);
}
