//! Argus guest agent (`argus-agent`): thin, read-mostly, dials outbound and
//! holds one persistent mTLS `Session` stream multiplexing everything (PRD
//! §2, §4). Built static against `x86_64-unknown-linux-musl` for Flatcar.

mod capabilities;
mod config;
mod docker;
mod enroll;
mod identity;
mod info;
mod logs;
mod metrics;
mod pty;
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
        // ANSI only when stdout is really a terminal: under systemd the
        // colour escapes land in journald verbatim, and journal viewers
        // half-strip them -- eating the characters around every field
        // ("INFOrgus_agent"). `fmt()` defaults to always-on.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    // rustls is pinned to the `ring` provider across the whole workspace.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // `--config <path>` is a restart-surviving fallback to the enroll page's
    // one-shot `sudo -n env VAR=...`; see `config::Config`'s doc.
    let args: Vec<String> = std::env::args().collect();
    let cfg = config::Config::load(&args)?;
    tracing::info!(endpoint = %cfg.endpoint, "argus-agent starting");

    // Enroll, then hold one persistent mTLS Session with reconnect/backoff
    // (PRD §5).
    let identity = enroll::ensure_enrolled(&cfg).await?;
    session::run(&cfg, identity).await
}
