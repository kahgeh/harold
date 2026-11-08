//! Tests of the factory flow page: the named query, the read, the levers and the page.

use super::flow::FACTORY_FLOW_SQL;

const STAGES: [&str; 8] = [
    "idea",
    "spec",
    "architect review",
    "approval",
    "build",
    "merge",
    "release",
    "installed",
];

fn queries_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/references/telemetry/queries")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn the_build_embeds_the_named_query_file() {
    assert_eq!(FACTORY_FLOW_SQL, queries_file("factory-flow.sql"));
}

#[test]
fn the_query_names_the_eight_stages_in_order() {
    let mut at = 0;
    for stage in STAGES {
        let literal = format!("'{stage}'");
        let found = FACTORY_FLOW_SQL[at..]
            .find(&literal)
            .unwrap_or_else(|| panic!("stage {stage} is missing or out of order"));
        at += found + literal.len();
    }
    assert!(FACTORY_FLOW_SQL.contains("ORDER BY ord"));
    assert!(FACTORY_FLOW_SQL.contains("-- Columns: stage, items, median_wait_s, has_data."));
}

#[test]
fn the_query_header_names_the_two_files_that_its_expressions_come_from() {
    let header: String = FACTORY_FLOW_SQL
        .lines()
        .take_while(|line| line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(header.contains("work-waiting.sql"));
    assert!(header.contains("delivery-lead-time.sql"));
}

#[test]
fn the_approval_and_build_expressions_match_the_rows_they_come_from() {
    let waiting = queries_file("work-waiting.sql");
    let lead = queries_file("delivery-lead-time.sql");
    // work-waiting.sql: the gauge, its name and its lists.
    assert!(waiting.contains("pod.inventory.items"));
    for list in [
        "awaiting_approval",
        "approved_not_started",
        "in_progress",
        "merged_not_released",
    ] {
        assert!(FACTORY_FLOW_SQL.contains(&format!("'{list}'")), "{list}");
    }
    assert!(FACTORY_FLOW_SQL.contains("MetricName = 'pod.inventory.items'"));
    assert!(FACTORY_FLOW_SQL.contains("TimeUnix > now() - INTERVAL 7 DAY"));
    // delivery-lead-time.sql: the whole rise rule of the "spec:" rows, line by line.
    let rise = [
        "if(cnt >= before_cnt, toInt64(cnt) - toInt64(before_cnt), toInt64(cnt)) AS rise_count,",
        "if(cnt >= before_cnt, total - before_total, total) AS rise_sum",
        "lagInFrame(Count, 1, toUInt64(0)) OVER (",
        "lagInFrame(Sum, 1, toFloat64(0)) OVER (",
        "PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)",
        "ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before_cnt,",
        "WHERE MetricName = 'pod.spec.lead_time'",
    ];
    for line in rise {
        assert!(lead.contains(line), "delivery-lead-time.sql lost: {line}");
        assert!(FACTORY_FLOW_SQL.contains(line), "factory-flow.sql lost: {line}");
    }
    for phase in ["open_to_approved", "approved_to_started", "started_to_completed"] {
        assert!(FACTORY_FLOW_SQL.contains(&format!("'{phase}'")), "{phase}");
    }
    assert!(FACTORY_FLOW_SQL.contains("Attributes['spec.id']"));
    assert!(FACTORY_FLOW_SQL.contains("INTERVAL 30 DAY"));
}

#[test]
fn the_query_has_no_product_filter_and_no_format_clause() {
    assert!(!FACTORY_FLOW_SQL.contains("$__filters"));
    assert!(!FACTORY_FLOW_SQL.contains("FORMAT"));
}
