//! Browser HTTP surface (PRD §9.1).
//!
//! Sits behind Traefik + cert-manager + Zitadel OIDC. Serves the embedded React
//! app with SPA fallback, plus health endpoints. The `/api`, SSE, and WebSocket
//! routes land with their slices.

use crate::config::Config;
use crate::embed::Assets;
use anyhow::Result;
use axum::{
    extract::State,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

/// Shared router state: the Postgres pool backing `/api` handlers.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

pub async fn serve(cfg: &Config, pool: PgPool) -> Result<()> {
    let app = router(AppState { pool });

    let listener = tokio::net::TcpListener::bind(&cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "browser HTTP surface listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the router without binding a socket, so tests can drive it directly
/// via `tower::ServiceExt::oneshot`.
fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/api/fleet", get(fleet))
        // TODO: nest remaining /api routes (machines, verbs, logs SSE, terminal
        // WS, events SSE, audit, enroll-tokens) and /auth OIDC routes here (PRD
        // §9.1).
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn readyz() -> impl IntoResponse {
    // TODO(spine): gate on Postgres connectivity so we don't accept agents before
    // migrations finish (PRD §2.5).
    (StatusCode::OK, "ready")
}

/// One row of the fleet page's machine table (PRD §9.1).
#[derive(serde::Serialize)]
struct FleetRow {
    id: Uuid,
    hostname: String,
    os: Option<String>,
    primary_ip: Option<String>,
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    last_seen_at: Option<OffsetDateTime>,
    tags: Vec<String>,
}

/// `GET /api/fleet` -- list every machine with its status, for the fleet page.
/// Intentionally UNAUTHENTICATED for the Spine slice: OIDC lands later, once
/// the browser surface moves behind Traefik + Zitadel (PRD §9.1).
async fn fleet(State(state): State<AppState>) -> Result<Json<Vec<FleetRow>>, StatusCode> {
    let rows = sqlx::query_as!(
        FleetRow,
        r#"SELECT id, hostname, os, host(primary_ip) as "primary_ip?", status,
                  last_seen_at, tags FROM machines ORDER BY hostname"#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "failed to list fleet");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(rows))
}

/// Serve the embedded React app, falling back to `index.html` for client-side
/// routes (PRD §10).
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(content) => (
            [(header::CONTENT_TYPE, content_type(path))],
            content.data.into_owned(),
        )
            .into_response(),
        None => match Assets::get("index.html") {
            Some(index) => (
                [(header::CONTENT_TYPE, "text/html")],
                index.data.into_owned(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    #[sqlx::test]
    async fn fleet_lists_machines_with_status(pool: PgPool) -> anyhow::Result<()> {
        // Hostnames are chosen so `ORDER BY hostname` deterministically puts
        // the online machine first and the offline one second.
        sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, os, primary_ip, status, last_seen_at)
               VALUES ('test-fleet-online', 'a-online-host', 'Debian 12', '10.0.0.5'::inet, 'online', now())"#
        )
        .execute(&pool)
        .await?;

        sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status)
               VALUES ('test-fleet-offline', 'z-offline-host', 'offline')"#
        )
        .execute(&pool)
        .await?;

        let app = router(AppState { pool });

        let response = app
            .oneshot(Request::builder().uri("/api/fleet").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["hostname"], "a-online-host");
        assert_eq!(rows[0]["status"], "online");
        assert_eq!(rows[1]["hostname"], "z-offline-host");
        assert_eq!(rows[1]["status"], "offline");

        Ok(())
    }
}
