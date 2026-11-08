//! The read of ClickHouse that Harold shares between the factory flow page and the evidence
//! check: the user `hyperdx_ro`, `readonly=1`, a 5-second limit and the configured URL only.
//! The password is read at each read and dropped after it.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::settings::expand_tilde;

/// The user that reads ClickHouse. It may only `SELECT` from `otel.*`.
pub(crate) const CLICKHOUSE_USER: &str = "hyperdx_ro";
/// The limit of the whole read, and of the query inside ClickHouse, in seconds.
pub(crate) const LIMIT_SECONDS: u64 = 5;

/// How a source failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceFailure {
    Unreachable,
    Refused,
    Timeout,
}

/// One query to send. It holds the password until it drops, and `Debug` never shows it.
pub(crate) struct ClickhouseRequest {
    url: String,
    password: String,
    sql: String,
    params: Vec<(String, String)>,
}

impl ClickhouseRequest {
    pub(crate) fn new(url: String, password: String, sql: impl Into<String>) -> Self {
        Self {
            url,
            password,
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// Adds the typed query parameter `name`.
    pub(crate) fn with_param(mut self, name: &str, value: &str) -> Self {
        self.params
            .push((format!("param_{name}"), value.to_string()));
        self
    }

    pub(crate) fn params(&self) -> &[(String, String)] {
        &self.params
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn user(&self) -> &'static str {
        CLICKHOUSE_USER
    }

    pub(crate) fn password(&self) -> &str {
        &self.password
    }

    pub(crate) fn sql(&self) -> &str {
        &self.sql
    }
}

impl fmt::Debug for ClickhouseRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClickhouseRequest")
            .field("url", &self.url)
            .field("password", &"<hidden>")
            .finish_non_exhaustive()
    }
}

/// Where the rows come from: ClickHouse in Harold, a fake in most tests. It answers with the
/// body in `JSONEachRow`.
pub(crate) trait ClickhouseSource: Send + Sync {
    fn query<'a>(
        &'a self,
        request: &'a ClickhouseRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>>;
}

/// ClickHouse over HTTP, with `reqwest`.
pub(crate) struct ReqwestSource {
    /// None when the client could not be built. Then no request goes out.
    client: Option<reqwest::Client>,
}

impl ReqwestSource {
    pub(crate) fn new() -> Self {
        Self::from_client(Self::client())
    }

    /// A source over `client`; with none, every query fails as `Unreachable`.
    pub(crate) fn from_client(client: Option<reqwest::Client>) -> Self {
        Self { client }
    }

    /// The client: no redirect and no proxy, so the password goes only to the configured URL.
    pub(crate) fn client() -> Option<reqwest::Client> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .ok()
    }

    /// The HTTP request for `request`: the user and the password in headers, the limits and
    /// the format in the query string, and the SQL as the body. The password is not in the URL.
    pub(crate) fn build(
        client: &reqwest::Client,
        request: &ClickhouseRequest,
    ) -> Result<reqwest::Request, reqwest::Error> {
        let mut key = reqwest::header::HeaderValue::from_str(request.password())
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""));
        key.set_sensitive(true);
        client
            .post(request.url())
            .query(request.params())
            .query(&[
                ("default_format", "JSONEachRow"),
                ("readonly", "1"),
                ("max_execution_time", "5"),
            ])
            .header("X-ClickHouse-User", request.user())
            .header("X-ClickHouse-Key", key)
            .timeout(Duration::from_secs(LIMIT_SECONDS))
            .body(request.sql().to_string())
            .build()
    }
}

impl ClickhouseSource for ReqwestSource {
    fn query<'a>(
        &'a self,
        request: &'a ClickhouseRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, SourceFailure>> + Send + 'a>> {
        Box::pin(async move {
            let client = self.client.as_ref().ok_or(SourceFailure::Unreachable)?;
            let built = Self::build(client, request).map_err(|_| SourceFailure::Refused)?;
            let response = client.execute(built).await.map_err(failure_of)?;
            if !response.status().is_success() {
                return Err(SourceFailure::Refused);
            }
            response.text().await.map_err(failure_of)
        })
    }
}

pub(crate) fn failure_of(error: reqwest::Error) -> SourceFailure {
    if error.is_timeout() {
        SourceFailure::Timeout
    } else {
        SourceFailure::Unreachable
    }
}

/// The `hyperdx_ro` password in the users file, by the rule of `bin/logq`: the text between the
/// quotes after `hyperdx_ro IDENTIFIED BY '`. None when it is missing or empty.
pub(crate) fn password_in(text: &str) -> Option<String> {
    const MARK: &str = "hyperdx_ro IDENTIFIED BY '";
    text.lines().find_map(|line| {
        let rest = &line[line.find(MARK)? + MARK.len()..];
        let password = &rest[..rest.find('\'')?];
        (!password.is_empty()).then(|| password.to_string())
    })
}

/// Why `fetch_body` gave no body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    /// No `users_file` setting.
    NotSetUp,
    Unreachable,
    /// The users file is missing, unreadable or holds no password, or ClickHouse refuses.
    Refused,
    /// The 5-second limit passed.
    Timeout,
}

/// Sends `sql` once, with the typed `params`, and gives the body in `JSONEachRow`. It reads the
/// password from `users_file` at this call and drops it before it returns.
pub(crate) async fn fetch_body(
    users_file: Option<&str>,
    url: &str,
    sql: &str,
    params: &[(&str, &str)],
    source: &dyn ClickhouseSource,
) -> Result<String, ReadFailure> {
    let users_file = users_file.ok_or(ReadFailure::NotSetUp)?;
    let text = tokio::fs::read_to_string(expand_tilde(users_file))
        .await
        .map_err(|_| ReadFailure::Refused)?;
    let password = password_in(&text).ok_or(ReadFailure::Refused)?;
    drop(text);
    let mut request = ClickhouseRequest::new(url.to_string(), password, sql);
    for (name, value) in params {
        request = request.with_param(name, value);
    }
    let limit = Duration::from_secs(LIMIT_SECONDS);
    let answer = tokio::time::timeout(limit, source.query(&request))
        .await
        .map_err(|_| ReadFailure::Timeout)?;
    drop(request);
    answer.map_err(|failure| match failure {
        SourceFailure::Unreachable => ReadFailure::Unreachable,
        SourceFailure::Refused => ReadFailure::Refused,
        SourceFailure::Timeout => ReadFailure::Timeout,
    })
}

#[cfg(test)]
#[path = "clickhouse_read_tests.rs"]
mod clickhouse_read_tests;
