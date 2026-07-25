//! Background work (PRD §7).
//!
//! Rule (see CLAUDE.md): survives-restart / retried / scheduled -> apalis; trivial
//! and loss-tolerant -> tokio task.
//!
//! apalis is intentionally NOT wired in the skeleton: the V1 "Spine" slice needs no
//! job queue, and `apalis-sql`'s Postgres backend must first pass a build-time
//! validation gate (retries + cron on the Postgres backend specifically; sqlx
//! version alignment) or fall back to `pgmq`. Until then the offline sweeper (this
//! task) and the future nightly metrics prune run from a tokio interval -- a
//! missed tick is harmless (the next tick catches up), which is exactly what
//! "loss-tolerant" means here.

use crate::repo;
use anyhow::Result;
use sqlx::PgPool;
use std::time::Duration;

/// Every 10s, flip any `online` machine not heard from in 45s (~3 missed 15s
/// heartbeats) to `offline`, then sweep expired browser sessions. Runs for the
/// lifetime of the process alongside the HTTP and gRPC surfaces (see
/// `main.rs`'s `try_join!`).
///
/// The session sweep is hygiene, not enforcement: `repo::lookup_session`
/// already filters on `expires_at > now()`, so an unswept row is never usable
/// by `auth::require_auth`. This tick just keeps the table from growing
/// unboundedly. No audit row is written for it -- CLAUDE.md's audit-log rule
/// applies to verbs a principal performs, and there is no principal here.
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

/// Every hour, delete `metrics` rows past the retention window
/// (`argus_common::METRICS_RETENTION_HOURS`, 48h). Runs for the lifetime of
/// the process alongside the HTTP and gRPC surfaces and the offline sweeper
/// (see `main.rs`'s `try_join!`). A missed tick is harmless -- the next tick
/// catches up -- which is exactly the "loss-tolerant" background-work rule.
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
