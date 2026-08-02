//! Background work (PRD §7). apalis isn't wired yet (pending its build-time
//! validation gate -- Postgres retries+cron, sqlx alignment -- or a `pgmq`
//! fallback; see CLAUDE.md), so the jobs below run from a loss-tolerant tokio
//! interval instead: a missed tick is harmless, the next tick catches up.

use crate::repo;
use anyhow::Result;
use sqlx::PgPool;
use std::time::Duration;

/// Every 10s: flip machines unheard-from in 45s (~3 missed 15s heartbeats)
/// to `offline`. Every 60th tick (~10 min), also sweep expired sessions.
/// Runs for the process lifetime alongside HTTP/gRPC (`main.rs`'s `try_join!`).
///
/// The offline sweep must stay tight (it drives the fleet page's status),
/// but the session sweep is hygiene, not enforcement -- `lookup_session`
/// already filters on `expires_at > now()`, so an unswept row is never
/// usable and there's no reason to DELETE-scan `sessions` every 10s. No
/// audit row for either: neither is a principal's verb.
pub async fn run(pool: PgPool) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    let mut tick: u64 = 0;
    loop {
        interval.tick().await;
        match repo::mark_stale_offline(&pool, Duration::from_secs(45)).await {
            Ok(n) if n > 0 => tracing::info!(count = n, "marked stale agents offline"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "offline sweep failed"),
        }
        // tick 0 (startup) sweeps too, so a restart never defers cleanup.
        if tick % 60 == 0 {
            match repo::delete_expired_sessions(&pool).await {
                Ok(n) if n > 0 => tracing::info!(count = n, "deleted expired sessions"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "session sweep failed"),
            }
        }
        tick = tick.wrapping_add(1);
    }
}

/// Every hour, deletes rows past their retention windows: `metrics`
/// (`argus_common::METRICS_RETENTION_HOURS`, 48h) and `audit_log`
/// (`argus_common::AUDIT_RETENTION_DAYS`, a year).
pub async fn prune_retention(pool: PgPool) -> Result<()> {
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
        match repo::prune_audit_log(
            &pool,
            Duration::from_secs(argus_common::AUDIT_RETENTION_DAYS as u64 * 86_400),
        )
        .await
        {
            Ok(n) if n > 0 => tracing::info!(count = n, "pruned old audit log entries"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "audit log prune failed"),
        }
    }
}
