//! The login check in front of every route. Tailscale Serve adds the
//! `Tailscale-User-Login` header to each request it forwards; a request without the
//! configured login is refused before any route sees it.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;

/// The header Tailscale Serve sets to the login of the person making the request.
pub(crate) const LOGIN_HEADER: &str = "tailscale-user-login";

/// Passes the request on when its login header equals `allowed_login` byte for byte.
/// Otherwise answers `403` with an empty body, and writes no log line, event or row.
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
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}
