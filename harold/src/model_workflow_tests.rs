use super::*;
use crate::settings::{ModelPrices, PricesSettings};
use crate::telemetry::{TurnTokens, test_support};
use std::path::{Path, PathBuf};

fn prices() -> PricesSettings {
    let mut prices = PricesSettings {
        exchange_rate_usd_to_aud: 1.5,
        ..PricesSettings::default()
    };
    prices.list.insert(
        "claude-test-1".to_owned(),
        ModelPrices {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.3,
        },
    );
    prices
}

fn tokens() -> TurnTokens {
    TurnTokens {
        input: 1_000_000,
        output: 100_000,
        cache_read: 0,
        cache_write: 0,
    }
}

#[test]
fn a_reported_cost_is_the_usd_value_times_the_rate() {
    let cost = cost_aud(&prices(), "claude-test-1", &tokens(), Some(0.01));
    assert!((cost.unwrap() - 0.015).abs() < 1e-9);
}

#[test]
fn a_result_with_no_cost_gets_tokens_times_list_price_times_rate() {
    // 1M input at 3 USD and 0.1M output at 15 USD: 4.5 USD, times 1.5.
    let cost = cost_aud(&prices(), "claude-test-1", &tokens(), None);
    assert!((cost.unwrap() - 6.75).abs() < 1e-9);
}

#[test]
fn a_model_with_no_price_and_no_reported_cost_gives_no_cost() {
    assert_eq!(cost_aud(&prices(), "claude-other-9", &tokens(), None), None);
}

#[test]
fn an_unpriced_model_is_logged_once_a_day() {
    let mut log = UnpricedLog::default();
    assert!(log.should_log("claude-other-9", 100));
    assert!(!log.should_log("claude-other-9", 100));
    assert!(log.should_log("claude-other-8", 100));
    assert!(log.should_log("claude-other-9", 101));
}

fn registry() -> Vec<(String, Vec<PathBuf>)> {
    vec![
        ("harold".to_owned(), vec![PathBuf::from("/dev/p/harold")]),
        (
            "keymap".to_owned(),
            vec![
                PathBuf::from("/dev/p/keymap"),
                PathBuf::from("/dev/p/harold/keymap"),
            ],
        ),
    ]
}

#[test]
fn a_folder_inside_a_match_path_gets_that_product() {
    let registry = registry();
    let found = product_for(Some(Path::new("/dev/p/keymap/src")), &registry);
    assert_eq!(found, "keymap");
}

#[test]
fn the_longest_match_path_wins() {
    let registry = registry();
    let found = product_for(Some(Path::new("/dev/p/harold/keymap/x")), &registry);
    assert_eq!(found, "keymap");
}

#[test]
fn a_folder_outside_every_match_path_gets_harold() {
    assert_eq!(
        product_for(Some(Path::new("/tmp/x")), &registry()),
        "harold"
    );
    assert_eq!(product_for(None, &registry()), "harold");
    // A name that only shares a prefix as text is not inside the path.
    assert_eq!(
        product_for(Some(Path::new("/dev/p/keymapper")), &registry()),
        "harold"
    );
}

#[test]
fn the_seed_has_each_pair_for_each_counter_with_model_none() {
    test_support::install();
    register_workflow_counters();
    let tokens = test_support::counter_snapshot("harold.workflow.tokens");
    for site in CALL_SITES {
        let base = [
            ("workflow", site.workflow),
            ("workflow.step", site.step),
            ("spec.id", "none"),
            ("model", "none"),
            ("pod.product", "harold"),
        ];
        let mut cost = base.to_vec();
        let found = test_support::counter_f64_value("harold.workflow.cost", &cost);
        assert_eq!(found, Some(0.0), "cost {}", site.workflow);
        for kind in ["input", "output", "cache_read", "cache_write"] {
            cost.push(("kind", kind));
            assert!(tokens.value(&cost).is_some(), "{} {kind}", site.workflow);
            cost.pop();
        }
    }
}

#[test]
fn the_list_has_six_model_workflows_and_not_the_restart_check() {
    assert_eq!(CALL_SITES.len(), 6);
    assert!(
        CALL_SITES
            .iter()
            .all(|site| site.workflow != "restart_check_memory")
    );
}

#[test]
fn record_adds_tokens_and_cost_with_the_labels() {
    test_support::install();
    configure(prices(), Vec::new());
    let spec = "wf-record-1";
    record(
        "watch",
        "run",
        Some(spec),
        None,
        "claude-test-1",
        &tokens(),
        None,
    );
    let labels = [
        ("spec.id", spec),
        ("workflow", "watch"),
        ("workflow.step", "run"),
    ];
    let mut input = labels.to_vec();
    input.push(("kind", "input"));
    input.push(("model", "claude-test-1"));
    input.push(("pod.product", "harold"));
    let snapshot = test_support::counter_snapshot("harold.workflow.tokens");
    assert_eq!(snapshot.value(&input), Some(1_000_000));
    let cost = test_support::counter_f64_value("harold.workflow.cost", &labels).unwrap();
    assert!((cost - 6.75).abs() < 1e-9);
}

#[test]
fn the_registry_match_paths_are_read_with_the_tilde_expanded() {
    let dir = std::env::temp_dir().join(format!("wf-match-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("products.toml");
    std::fs::write(
        &file,
        "[products.a]\nrepo = \"/a\"\nbase = \"main\"\nmatch = [\"~/x\", \"/y\"]\n\n\
         [products.b]\nrepo = \"/b\"\nbase = \"main\"\n",
    )
    .unwrap();
    let found = crate::pod_stats::read_match_paths(&file).unwrap();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        found,
        vec![
            (
                "a".to_owned(),
                vec![PathBuf::from(format!("{home}/x")), PathBuf::from("/y")]
            ),
            ("b".to_owned(), Vec::new()),
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn record_output_reads_usage_cost_and_model_from_the_json_result() {
    test_support::install();
    configure(prices(), Vec::new());
    let stdout = br#"{"type":"result","result":"x","total_cost_usd":0.01,
        "usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,
                 "cache_creation_input_tokens":40},
        "modelUsage":{"claude-wf-json-1":{"inputTokens":10}}}"#;
    record_output(
        "call_answer",
        "answer",
        Some("wf-json-1"),
        None,
        "haiku",
        stdout,
    );
    let labels = [("spec.id", "wf-json-1"), ("workflow", "call_answer")];
    let snapshot = test_support::counter_snapshot("harold.workflow.tokens");
    for (kind, count) in [
        ("input", 10),
        ("output", 20),
        ("cache_read", 30),
        ("cache_write", 40),
    ] {
        let wanted = [
            labels[0],
            labels[1],
            ("workflow.step", "answer"),
            ("pod.product", "harold"),
            ("kind", kind),
            ("model", "claude-wf-json-1"),
        ];
        assert_eq!(snapshot.value(&wanted), Some(count), "{kind}");
    }
    let cost = test_support::counter_f64_value("harold.workflow.cost", &labels).unwrap();
    assert!((cost - 0.015).abs() < 1e-9);
}

#[test]
fn record_output_with_unreadable_output_records_nothing() {
    test_support::install();
    record_output(
        "call_answer",
        "answer",
        Some("wf-json-2"),
        None,
        "claude-x",
        b"not json",
    );
    let snapshot = test_support::counter_snapshot("harold.workflow.tokens");
    assert_eq!(snapshot.value(&[("spec.id", "wf-json-2")]), None);
}

// ---------------------------------------------------------------------------
// The call-site test
// ---------------------------------------------------------------------------

/// Whether the text runs a model: it holds the argument `"--print"` or `"--max-turns"`, or
/// it defines or implements a `ModelRunner`.
fn is_model_file(text: &str) -> bool {
    text.contains("\"--print\"")
        || text.contains("\"--max-turns\"")
        || text.contains("trait ModelRunner")
        || text
            .lines()
            .any(|line| line.trim_start().starts_with("impl") && line.contains("ModelRunner for"))
}

/// What is wrong with the list `sites` for these files, as one line each: a model file in
/// no entry, an entry whose file is missing, and an entry whose record file has no `record`
/// call with the name of its model workflow.
fn call_site_problems(files: &[(String, String)], sites: &[CallSite]) -> Vec<String> {
    let text_of = |path: &str| {
        files
            .iter()
            .find(|(name, _)| name == path)
            .map(|(_, text)| text.as_str())
    };
    let mut problems = Vec::new();
    for (path, text) in files {
        if is_model_file(text) && !sites.iter().any(|site| site.model_file == path) {
            problems.push(format!(
                "{path} runs a model and is in no entry of CALL_SITES"
            ));
        }
    }
    for site in sites {
        if text_of(site.model_file).is_none() {
            problems.push(format!(
                "{}: model file {} not found",
                site.workflow, site.model_file
            ));
        }
        let recorded = text_of(site.record_file).is_some_and(|text| {
            text.contains("model_workflow::record")
                && text.contains(&format!("\"{}\"", site.workflow))
        });
        if !recorded {
            problems.push(format!(
                "{}: {} has no record call with the name \"{}\"",
                site.workflow, site.record_file, site.workflow
            ));
        }
    }
    problems
}

/// The text up to the line `#[cfg(test)]` that opens the test module: the next line is
/// `mod ...` or `#[path ...]`. Another `#[cfg(test)]` item before it stays.
fn before_test_module(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let opens = |at: usize| {
        lines[at].trim() == "#[cfg(test)]"
            && lines.get(at + 1).is_some_and(|next| {
                let next = next.trim_start();
                next.starts_with("mod ") || next.starts_with("#[path")
            })
    };
    let end = (0..lines.len())
        .find(|&at| opens(at))
        .unwrap_or(lines.len());
    lines[..end].join("\n")
}

/// Every non-test `.rs` file under `dir`, with its path under `dir` and its text before the
/// test module. It skips the test files, the test support and `model_workflow.rs`.
fn source_files(dir: &Path) -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "test_support" {
                    walk(root, &path, found);
                }
                continue;
            }
            let skipped = !name.ends_with(".rs")
                || name.ends_with("_tests.rs")
                || matches!(
                    name.as_str(),
                    "tests.rs" | "test_support.rs" | "model_workflow.rs"
                );
            if skipped {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let text = before_test_module(&text);
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            found.push((relative, text));
        }
    }
    let mut found = Vec::new();
    walk(dir, dir, &mut found);
    found
}

fn file(path: &str, text: &str) -> (String, String) {
    (path.to_owned(), text.to_owned())
}

const ENTRY: &[CallSite] = &[CallSite {
    workflow: "demo",
    step: "go",
    model_file: "a.rs",
    record_file: "b.rs",
}];

#[test]
fn a_model_file_in_no_entry_is_reported() {
    let files = [
        file("a.rs", "let args = [\"--print\"];"),
        file("b.rs", "model_workflow::record_output(\"demo\", \"go\");"),
        file("c.rs", "let args = [\"--max-turns\", \"1\"];"),
    ];
    let problems = call_site_problems(&files, ENTRY);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("c.rs"));
}

#[test]
fn a_model_runner_trait_or_impl_makes_a_model_file() {
    let files = [
        file("a.rs", "pub trait ModelRunner: Send {}"),
        file("b.rs", "model_workflow::record(\"demo\", \"go\");"),
        file("d.rs", "impl ModelRunner for Fake {}"),
    ];
    let problems = call_site_problems(&files, ENTRY);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("d.rs"));
}

#[test]
fn an_entry_whose_record_file_has_no_record_call_is_reported() {
    let files = [
        file("a.rs", "let args = [\"--print\"];"),
        file("b.rs", "fn other() { record_nothing(\"demo\"); }"),
    ];
    let problems = call_site_problems(&files, ENTRY);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("demo"));
}

#[test]
fn a_record_call_for_another_workflow_does_not_count() {
    let files = [
        file("a.rs", "let args = [\"--print\"];"),
        file("b.rs", "model_workflow::record_output(\"other\", \"go\");"),
    ];
    assert_eq!(call_site_problems(&files, ENTRY).len(), 1);
}

#[test]
fn text_after_the_test_module_marker_is_not_read() {
    let files = [
        file("a.rs", "let args = [\"--print\"];"),
        file("b.rs", "model_workflow::record(\"demo\", \"go\");"),
        file(
            "e.rs",
            "fn x() {}\n#[cfg(test)]\nmod t { impl ModelRunner for F {} }",
        ),
    ];
    assert!(call_site_problems(&files, ENTRY).is_empty());
}

#[test]
fn every_model_call_in_harold_src_is_in_the_list_and_records() {
    let files = source_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    assert!(files.len() > 100, "the scan read {} files", files.len());
    let problems = call_site_problems(&files, CALL_SITES);
    assert!(problems.is_empty(), "{problems:#?}");
}
