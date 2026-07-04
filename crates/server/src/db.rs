use crate::config::Config;
use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect to Postgres. Readiness is gated on this succeeding (PRD §2.5).
pub async fn connect(cfg: &Config) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await?;
    Ok(pool)
}

/// Run embedded migrations on startup -- no init container (PRD §6). The macro
/// reads `crates/server/migrations/` at compile time and embeds the SQL.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}
