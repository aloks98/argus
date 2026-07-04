//! Background work (PRD §7).
//!
//! Rule (see CLAUDE.md): survives-restart / retried / scheduled -> apalis; trivial
//! and loss-tolerant -> tokio task.
//!
//! apalis is intentionally NOT wired in the skeleton: the V1 "Spine" slice needs no
//! job queue, and `apalis-sql`'s Postgres backend must first pass a build-time
//! validation gate (retries + cron on the Postgres backend specifically; sqlx
//! version alignment) or fall back to `pgmq`. Until then the nightly metrics prune
//! runs from a tokio interval.

#![allow(dead_code)]

use crate::config::Config;
use anyhow::Result;

pub async fn run(_cfg: &Config) -> Result<()> {
    // TODO(metrics slice): nightly `DELETE FROM metrics WHERE ts < now() - 48h`.
    // TODO(V1.1): introduce apalis (or pgmq) for script runs + scheduled tasks.
    std::future::pending::<()>().await;
    Ok(())
}
