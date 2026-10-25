//! The checks after the login check, against web pages that borrow the browser's access.
//!
//! The login header is added by Tailscale Serve from the device a request comes from, so
//! any page open in the phone's browser carries it, as it would a cookie. Two checks keep
//! such pages out:
//!
//! - **The Host:** a page that rebinds its own name to `127.0.0.1` reaches the listener as
//!   the same origin and can set the login header itself. Only the tailnet's `.ts.net`
//!   names and the app's loopback address are answered.
//! - **Posts from the app's own page:** a post must be same-origin and carry a header a
//!   page on another site could send only after a preflight, which Harold never grants.
//!   Every post and delete from a page goes through Datastar, which sends
//!   `Datastar-Request: true`. Two routes keep their own transport, and each is let in by
//!   its own content type: the call's raw audio and the browser's trace exporter (ADR 0018).

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use super::AppState;

/// The one route that takes `application/octet-stream` without the Datastar header: the
/// call's raw audio, which as signals would be base64 and a third larger.
const AUDIO_ROUTE: &str = "/call/audio";
/// The one route that takes `application/json` without the Datastar header: the
/// OpenTelemetry library's own exporter posts its spans there.
const TRACES_ROUTE: &str = "/telemetry/traces";

/// The tailnet's names: Tailscale Serve passes the phone's `https://<machine>.<tailnet>.ts.net`
/// Host through.
const TAILNET_SUFFIX: &str = ".ts.net";

/// Passes the request on when its Host is accepted and, for any method other than GET or
/// HEAD, it comes from the app's own page. Otherwise answers `403` with an empty body, and
/// writes no event or row. A post from the app's own origin that has no Datastar header,
/// and is not one of the two routes with their own transport, logs `app post refused`
/// with its route and `not_datastar`; any other refusal writes no log line.
pub(crate) async fn require_same_origin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    // HTTP/2 carries the Host in the request's authority instead of the header.
    let host = header_text(headers, header::HOST).or_else(|| {
        request
            .uri()
            .authority()
            .map(|authority| authority.as_str())
    });
    let Some(host) = host else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !host_is_accepted(host, &state.loopback_hosts) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let reads_only = matches!(*request.method(), Method::GET | Method::HEAD);
    if !reads_only {
        let same_origin = is_same_origin(headers, host);
        if !(same_origin && has_non_simple_header(headers, request.uri().path())) {
            if same_origin {
                warn!(
                    route = request.uri().path(),
                    reason = "not_datastar",
                    "app post refused"
                );
            }
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(request).await
}

/// The loopback names the app answers to: the listen address as written in the settings,
/// the address actually bound, and `localhost` with the bound port. Lowercase.
pub(crate) fn loopback_hosts(listen: &str, bound: std::net::SocketAddr) -> Vec<String> {
    let mut hosts = vec![
        listen.to_ascii_lowercase(),
        bound.to_string(),
        format!("localhost:{}", bound.port()),
    ];
    hosts.dedup();
    hosts
}

/// True for one of `loopback_hosts`, or a `.ts.net` name with or without a port, compared
/// without regard to case.
fn host_is_accepted(host: &str, loopback_hosts: &[String]) -> bool {
    if loopback_hosts
        .iter()
        .any(|accepted| accepted.eq_ignore_ascii_case(host))
    {
        return true;
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    name.len() > TAILNET_SUFFIX.len()
        && name
            .get(name.len() - TAILNET_SUFFIX.len()..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(TAILNET_SUFFIX))
}

/// True when the browser says the request comes from this origin: `Sec-Fetch-Site` is
/// `same-origin`, or, without it, `Origin` is `https://` or `http://` and the Host.
fn is_same_origin(headers: &HeaderMap, host: &str) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site.as_bytes() == b"same-origin";
    }
    let Some(origin) = header_text(headers, header::ORIGIN) else {
        return false;
    };
    ["https://", "http://"].iter().any(|scheme| {
        origin
            .strip_prefix(scheme)
            .is_some_and(|rest| rest.eq_ignore_ascii_case(host))
    })
}

/// True for `Datastar-Request: true`, which Datastar's `@post` and `@delete` send. Without
/// it, true only for `application/octet-stream` on `AUDIO_ROUTE` and `application/json` on
/// `TRACES_ROUTE`. A page on another site cannot send any of them without a preflight.
fn has_non_simple_header(headers: &HeaderMap, path: &str) -> bool {
    let datastar = headers
        .get("datastar-request")
        .is_some_and(|value| value.as_bytes() == b"true");
    if datastar {
        return true;
    }
    let wanted = match path {
        AUDIO_ROUTE => "application/octet-stream",
        TRACES_ROUTE => "application/json",
        _ => return false,
    };
    header_text(headers, header::CONTENT_TYPE).is_some_and(|content_type| {
        content_type
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(wanted))
    })
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}
