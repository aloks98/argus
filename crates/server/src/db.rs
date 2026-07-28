use crate::config::Config;
use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect to Postgres. Readiness is gated on this succeeding.
pub async fn connect(cfg: &Config) -> Result<PgPool> {
    connect_url(&cfg.database_url).await
}

/// Connect to Postgres from a bare database URL, with no `Config` involved.
///
/// Used by `argus local-admin reset` (main.rs), which must work even when
/// the rest of `Config::from_env` -- OIDC vars, `ARGUS_FIELD_KEY`, etc. -- is
/// absent or wrong. `connect` is defined in terms of this so there remains
/// exactly one place that opens a pool.
pub async fn connect_url(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Run embedded migrations on startup -- no init container. The macro reads
/// `crates/server/migrations/` at compile time and embeds the SQL.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}
