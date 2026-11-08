//! Tests of the shared ClickHouse read.

use super::{ClickhouseRequest, ReqwestSource};

#[test]
fn a_parameter_goes_in_the_query_string_as_param_name_and_not_in_the_sql_text() {
    let request = ClickhouseRequest::new(
        "http://127.0.0.1:8123/".to_string(),
        "fake-password".to_string(),
        "SELECT 1 WHERE t > {since:DateTime}",
    )
    .with_param("since", "2026-11-12 03:05:00");
    let built = ReqwestSource::build(&reqwest::Client::new(), &request).expect("build");
    let pairs: Vec<(String, String)> = built
        .url()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert!(pairs.contains(&("param_since".to_string(), "2026-11-12 03:05:00".to_string())));
    let body = built.body().and_then(|b| b.as_bytes()).expect("body");
    assert_eq!(body, b"SELECT 1 WHERE t > {since:DateTime}");
    assert!(!String::from_utf8_lossy(body).contains("2026-11-12"));
}
