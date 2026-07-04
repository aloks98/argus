//! Browser HTTP surface (PRD §9.1).
//!
//! Sits behind Traefik + cert-manager + Zitadel OIDC. Serves the embedded React
//! app with SPA fallback, plus health endpoints. The `/api`, SSE, and WebSocket
//! routes land with their slices.

use crate::config::Config;
use crate::embed::Assets;
use anyhow::Result;
use axum::{
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower_http::trace::TraceLayer;

pub async fn serve(cfg: &Config) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        // TODO: nest /api routes (fleet, machines, verbs, logs SSE, terminal WS,
        // events SSE, audit, enroll-tokens) and /auth OIDC routes here (PRD §9.1).
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "browser HTTP surface listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn readyz() -> impl IntoResponse {
    // TODO(spine): gate on Postgres connectivity so we don't accept agents before
    // migrations finish (PRD §2.5).
    (StatusCode::OK, "ready")
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
