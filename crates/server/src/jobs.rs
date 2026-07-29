//! Background work (PRD §7). apalis isn't wired yet (pending its build-time
//! validation gate -- Postgres retries+cron, sqlx alignment -- or a `pgmq`
//! fallback; see CLAUDE.md), so the jobs below run from a loss-tolerant tokio
//! interval instead: a missed tick is harmless, the next tick catches up.

use crate::repo;
use anyhow::Result;
use sqlx::PgPool;
use std::time::Duration;

/// Every 10s: flip machines unheard-from in 45s (~3 missed 15s heartbeats) to
/// `offline`, then sweep expired sessions. Runs for the process lifetime
/// alongside HTTP/gRPC (`main.rs`'s `try_join!`).
///
/// The session sweep is hygiene, not enforcement -- `lookup_session` already
/// filters on `expires_at > now()`. No audit row: this isn't a principal's verb.
pub async fn run(pool: PgPool) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        match repo::mark_stale_offline(&pool, Duration::from_secs(45)).await {
            Ok(n) if n > 0 => tracing::info!(count = n, "marked stale agents offline"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "offline sweep failed"),
        }
        match repo::delete_expired_sessions(&pool).await {
            Ok(n) if n > 0 => tracing::info!(count = n, "deleted expired sessions"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "session sweep failed"),
        }
    }
}

/// Every hour, deletes `metrics` rows past the retention window
/// (`argus_common::METRICS_RETENTION_HOURS`, 48h).
pub async fn prune_metrics(pool: PgPool) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        match repo::prune_metrics(
            &pool,
            Duration::from_secs(argus_common::METRICS_RETENTION_HOURS as u64 * 3600),
        )
        .await
        {
            Ok(n) if n > 0 => tracing::info!(count = n, "pruned old metrics"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "metrics prune failed"),
        }
    }
}
