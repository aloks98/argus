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
mod crypto;
mod db;
mod embed;
mod grpc;
mod http;
mod identity;
mod jobs;
mod repo;

use anyhow::Result;
use std::sync::Arc;

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

    // Load (or generate + persist) the internal CA, then issue the control
    // plane's own TLS leaf for the agent gRPC surface (Spine, build slice #1).
    let field_cipher = crypto::FieldCipher::from_b64_key(&cfg.field_key_b64)?;
    let ca = Arc::new(ca::CertAuthority::load_or_init(&pool, &field_cipher).await?);
    let server_identity = ca.issue_server_cert(&cfg.agent_sans)?;

    let agent_svc = grpc::AgentSvc::new(ca, pool.clone());

    // Serve the browser HTTP surface, the agent gRPC surface, the offline
    // sweeper, and the hourly metrics-retention prune concurrently.
    tokio::try_join!(
        http::serve(&cfg, pool.clone()),
        grpc::serve(&cfg, agent_svc, server_identity),
        jobs::run(pool.clone()),
        jobs::prune_metrics(pool.clone()),
    )?;

    Ok(())
}
