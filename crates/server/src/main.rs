//! Argus control plane (`argus`).
//!
//! Stateless binary: all state lives in Postgres (PRD §2). It serves two network
//! surfaces — the browser HTTP surface (behind Traefik) and the agent mTLS gRPC
//! surface (behind a dedicated MetalLB LoadBalancer). See docs/PRD.md.
//!
//! This is the skeleton: the browser surface (health + embedded UI) is wired; the
//! agent plane (CA + mTLS gRPC) and background jobs are build slice #1+ and land
//! during implementation. Modules below carry the intended shape.

mod ca;
mod config;
mod db;
mod embed;
mod grpc;
mod http;
mod jobs;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,argus=debug".into()),
        )
        .init();

    // rustls is pinned to the `ring` provider across the whole workspace.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = config::Config::from_env()?;
    tracing::info!(http_addr = %cfg.http_addr, agent_addr = %cfg.agent_addr, "starting argus control plane");

    // Readiness is gated on Postgres: connect and migrate before serving (PRD §2.5).
    let pool = db::connect(&cfg).await?;
    db::migrate(&pool).await?;
    tracing::info!("migrations applied");

    // Build slice #1 (Spine) continues here: load/create the CA (`ca`), then serve
    // the agent mTLS gRPC surface (`grpc`) alongside the browser surface, and start
    // background jobs (`jobs`). Until wired, only the browser surface runs.
    tracing::warn!(
        "agent gRPC surface, internal CA, and jobs are not yet wired -- see docs/PRD.md §5, §8"
    );

    http::serve(&cfg).await
}
