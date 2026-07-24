//! Argus guest agent (`argus-agent`).
//!
//! Thin, read-mostly. Dials outbound to the control plane and holds one persistent
//! mTLS `Session` stream over which everything is multiplexed (PRD §2, §4). Built
//! static against `x86_64-unknown-linux-musl` for release so it runs on Flatcar.
//!
//! This is the skeleton: the enrollment handshake (slice #1) and session loop are
//! stubbed with their intended shape.

mod capabilities;
mod config;
mod docker;
mod enroll;
mod identity;
mod info;
mod logs;
mod metrics;
mod session;
mod systemd;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,argus_agent=debug".into()),
        )
        .init();

    // rustls is pinned to the `ring` provider across the whole workspace.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = config::Config::from_env()?;
    tracing::info!(endpoint = %cfg.endpoint, "argus-agent starting");

    // Build slice #1 (Spine): ensure enrolled (obtain a client cert), then hold a
    // single persistent mTLS Session, reconnecting with backoff + jitter and
    // re-sending a Hello snapshot on reconnect (PRD §5).
    let identity = enroll::ensure_enrolled(&cfg).await?;
    session::run(&cfg, identity).await
}
