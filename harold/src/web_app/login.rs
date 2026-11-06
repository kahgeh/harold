//! The login check in front of every route. Tailscale Serve adds the
//! `Tailscale-User-Login` header to each request it forwards; a request without the
//! configured login is refused before any route sees it.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use tracing::info;

use super::AppState;
use super::request_signals::{refuse, route_class};

/// The header Tailscale Serve sets to the login of the person making the request.
pub(crate) const LOGIN_HEADER: &str = "tailscale-user-login";

/// Passes the request on when its login header equals `allowed_login` byte for byte.
/// Otherwise answers `403` with an empty body, marked as the refusal `no_login`. It writes
/// no event or row. It logs `app login refused` with the route class, once per hour.
pub(crate) async fn require_login(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let allowed = request
        .headers()
        .get(LOGIN_HEADER)
        .is_some_and(|login| login.as_bytes() == state.allowed_login.as_bytes());
    if !allowed {
        if state.pairing.should_log("login") {
            info!(
                route_class = route_class(request.uri().path()),
                "app login refused"
            );
        }
        return refuse(StatusCode::FORBIDDEN, "no_login");
    }
    next.run(request).await
}
