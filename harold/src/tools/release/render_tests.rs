use harold_api::tools::release::v1::{Line, NextResponse, ProductNext};

use super::{json, text};

fn line(id: &str, title: &str) -> Line {
    Line {
        id: id.into(),
        title: title.into(),
        commits: 0,
        commits_known: false,
    }
}

fn harold() -> ProductNext {
    ProductNext {
        name: "harold".into(),
        release: "v0.73.0".into(),
        merged: vec![line("timing-tests-local", "Run the tests locally")],
        in_progress: vec![Line {
            commits: 4,
            commits_known: true,
            ..line("row-names-plain", "Name each row")
        }],
        approved: vec![line("feature-build-gaps", "Hold back messages, …")],
        parked: vec![
            line("dependency-audit-guard", ""),
            line("linux-satellite-2b", ""),
        ],
        awaiting: vec![line("reply-once", "Make a reply reach the chief")],
        not_clear: vec![line("harold-agent-metrics", "")],
        ..Default::default()
    }
}

fn keymap() -> ProductNext {
    ProductNext {
        name: "keymap".into(),
        merged: vec![
            line("pane-numbers-docs", "Record the review"),
            line("same-keys", "Make the keys the same"),
        ],
        awaiting: vec![line("rex-direct-chords", "Add the direct chords")],
        ..Default::default()
    }
}

fn buckwal() -> ProductNext {
    ProductNext {
        name: "buckwal".into(),
        paused: true,
        ..Default::default()
    }
}

#[test]
fn the_evidence_missing_list_follows_the_merged_list_and_an_empty_list_prints_nothing() {
    let mut product = harold();
    let plain = text(&NextResponse {
        products: vec![product.clone()],
    });
    assert!(!plain.contains("evidence missing"), "{plain}");
    product.evidence_missing = vec![line("alpha", "First"), line("beta", "")];
    let shown = text(&NextResponse {
        products: vec![product.clone()],
    });
    assert!(
        shown.contains(
            "    timing-tests-local  Run the tests locally\n  evidence missing\n    alpha  First\n    beta\n  in progress\n"
        ),
        "{shown}"
    );
    let value: serde_json::Value = serde_json::from_str(&json(&NextResponse {
        products: vec![product],
    }))
    .unwrap();
    assert_eq!(value["products"][0]["evidence_missing"][0]["id"], "alpha");
    assert_eq!(value["products"][0]["evidence_missing"][1]["id"], "beta");
}

#[test]
fn text_matches_the_spec_example() {
    let response = NextResponse {
        products: vec![harold(), keymap(), buckwal()],
    };
    let expected = "\
harold
  merged since the release (v0.73.0)
    timing-tests-local  Run the tests locally
  in progress
    row-names-plain  Name each row  (4 commits)
  approved, not started
    feature-build-gaps  Hold back messages, …
  parked
    dependency-audit-guard
    linux-satellite-2b
  awaiting approval
    reply-once  Make a reply reach the chief
  not clear (in progress, no branch)
    harold-agent-metrics
  other commits since v0.73.0: 0

keymap
  no release yet: merged in the last 14 days
    pane-numbers-docs  Record the review
    same-keys  Make the keys the same
  in progress: none
  approved, not started: none
  awaiting approval
    rex-direct-chords  Add the direct chords

buckwal
  paused: no release yet
";
    let actual = text(&response);
    for (number, (want, got)) in expected.lines().zip(actual.lines()).enumerate() {
        assert_eq!(want, got, "line {}", number + 1);
    }
    assert_eq!(expected.lines().count(), actual.lines().count());
}

#[test]
fn a_paused_product_prints_one_line() {
    let mut product = buckwal();
    product.release = "v1.2.0".into();
    let actual = text(&NextResponse {
        products: vec![product],
    });
    assert_eq!(actual, "buckwal\n  paused: last release v1.2.0\n");
}

#[test]
fn no_release_uses_the_14_day_heading() {
    let actual = text(&NextResponse {
        products: vec![keymap()],
    });
    assert!(actual.contains("  no release yet: merged in the last 14 days\n"));
    assert!(!actual.contains("other commits"));
}

#[test]
fn a_product_with_no_git_answer_prints_its_reason() {
    let product = ProductNext {
        name: "harold".into(),
        unavailable: "git gave no answer".into(),
        ..Default::default()
    };
    let actual = text(&NextResponse {
        products: vec![product, keymap()],
    });
    assert!(actual.starts_with("harold\n  unavailable: git gave no answer\n\nkeymap\n"));
}

#[test]
fn json_holds_the_same_lists() {
    let value: serde_json::Value = serde_json::from_str(&json(&NextResponse {
        products: vec![harold()],
    }))
    .expect("valid JSON");
    let product = &value["products"][0];
    assert_eq!(product["name"], "harold");
    assert_eq!(product["release"], "v0.73.0");
    assert_eq!(product["paused"], false);
    assert_eq!(product["merged"][0]["id"], "timing-tests-local");
    assert_eq!(product["in_progress"][0]["commits"], 4);
    assert!(product["merged"][0].get("commits").is_none());
    assert_eq!(product["parked"][1]["id"], "linux-satellite-2b");
    assert_eq!(
        product["awaiting"][0]["title"],
        "Make a reply reach the chief"
    );
    assert_eq!(product["not_clear"][0]["id"], "harold-agent-metrics");
    assert_eq!(product["other_commits"], 0);
}

#[test]
fn in_progress_line_with_no_known_count_shows_no_suffix() {
    let product = ProductNext {
        name: "keymap".into(),
        in_progress: vec![line("alpha", "First")],
        ..Default::default()
    };

    let actual = text(&NextResponse {
        products: vec![product],
    });

    assert!(actual.contains("    alpha  First\n"), "{actual}");
    assert!(!actual.contains("commits)"), "{actual}");
    let value: serde_json::Value = serde_json::from_str(&json(&NextResponse {
        products: vec![ProductNext {
            in_progress: vec![line("alpha", "First")],
            ..Default::default()
        }],
    }))
    .unwrap();
    assert!(
        value["products"][0]["in_progress"][0]
            .get("commits")
            .is_none()
    );
}

#[test]
fn in_progress_line_with_a_known_zero_shows_zero_commits() {
    let known = Line {
        commits_known: true,
        ..line("alpha", "First")
    };
    let response = NextResponse {
        products: vec![ProductNext {
            name: "keymap".into(),
            in_progress: vec![known],
            ..Default::default()
        }],
    };

    assert!(text(&response).contains("    alpha  First  (0 commits)\n"));
    let value: serde_json::Value = serde_json::from_str(&json(&response)).unwrap();
    assert_eq!(value["products"][0]["in_progress"][0]["commits"], 0);
}
