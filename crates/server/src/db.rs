use crate::config::Config;
use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect to Postgres. Readiness is gated on this succeeding (PRD §2.5).
pub async fn connect(cfg: &Config) -> Result<PgPool> {
    connect_url(&cfg.database_url).await
}

/// Connect to Postgres from a bare database URL, with no `Config` involved.
///
/// This exists for `argus local-admin reset` (main.rs): the CLI is the
/// recovery path for when the REST of the configuration -- OIDC vars,
/// `ARGUS_FIELD_KEY`, `ARGUS_PUBLIC_URL` -- is absent or wrong, so it must not
/// depend on `Config::from_env` succeeding. Building a `Config` with dummy
/// values for the fields it doesn't need would defeat that point, so it gets
/// its own entry point instead; `connect` is defined in terms of it so there
/// is still exactly one place that opens a pool.
pub async fn connect_url(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Run embedded migrations on startup -- no init container (PRD §6). The macro
/// reads `crates/server/migrations/` at compile time and embeds the SQL.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}
