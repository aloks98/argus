//! Browser authentication: middleware, extractor, and the OIDC flow.

pub mod claims;
pub mod local;
pub mod oidc;
pub mod password;
pub mod session;

use crate::http::AppState;
use crate::repo::{self, Identity};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::extract::cookie::CookieJar;

/// Resolves the session cookie to an `Identity` and stores it in the request
/// extensions. Applied to the whole `/api` router, so it covers plain JSON,
/// SSE and the terminal WebSocket in one place -- cookies ride the upgrade
/// request, so no transport needs special handling.
pub async fn require_auth(
    State(state): State<AppState>,
    jar: CookieJar,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = jar
        .get(argus_common::SESSION_COOKIE)
        .map(|c| c.value().to_string())
    else {
        return unauthenticated();
    };

    match repo::lookup_session(&state.pool, &session::hash_token(&token)).await {
        Ok(Some(identity)) => {
            req.extensions_mut().insert(identity);
            next.run(req).await
        }
        Ok(None) => unauthenticated(),
        Err(e) => {
            // A database failure must not read as "signed out", or the UI will
            // send the operator to a login screen that cannot help them.
            tracing::error!(error = %e, "session lookup failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed").into_response()
        }
    }
}

fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

/// Handler extractor for the authenticated user.
///
/// Infallible in practice: it can only be used on routes behind
/// `require_auth`, which inserts the extension. The rejection exists so a
/// future route mounted outside the layer fails loudly rather than silently
/// attributing an action to nobody.
pub struct AuthUser(pub Identity);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Identity>()
            .cloned()
            .map(AuthUser)
            .ok_or_else(unauthenticated)
    }
}
