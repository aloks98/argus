use crate::config::Config;
use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Readiness is gated on this succeeding (PRD §2.5).
pub async fn connect(cfg: &Config) -> Result<PgPool> {
    connect_url(&cfg.database_url).await
}

/// Bare-URL variant, no `Config` needed: used by `argus local-admin reset`,
/// which must work even when `Config::from_env` (OIDC vars, `ARGUS_FIELD_KEY`,
/// etc.) is absent or wrong. `connect` wraps this so there's one place that
/// opens a pool.
pub async fn connect_url(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// No init container (PRD §6).
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}
