//! Argus control plane (`argus`).
//!
//! Stateless binary: all state lives in Postgres (PRD §2). It serves two network
//! surfaces — the browser HTTP surface (behind Traefik) and the agent mTLS gRPC
//! surface (behind a dedicated MetalLB LoadBalancer). See docs/PRD.md.
//!
//! This is the skeleton: the browser surface (health + embedded UI) is wired; the
//! agent plane (CA + mTLS gRPC) and background jobs are build slice #1+ and land
//! during implementation. Modules below carry the intended shape.

mod auth;
mod ca;
mod config;
mod crypto;
mod db;
mod embed;
mod grpc;
mod http;
mod hub;
mod identity;
mod jobs;
mod repo;
mod terminal;

use anyhow::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,argus=debug".into()),
        )
        .init();

    // rustls is pinned to the `ring` provider across the whole workspace.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = config::Config::from_env()?;
    tracing::info!(http_addr = %cfg.http_addr, agent_addr = %cfg.agent_addr, "starting argus control plane");

    // Readiness is gated on Postgres: connect and migrate before serving (PRD §2.5).
    let pool = db::connect(&cfg).await?;
    db::migrate(&pool).await?;
    tracing::info!("migrations applied");

    // Design §4: the invariant is "authentication is configured", not "OIDC is
    // configured". Booting with neither would serve the browser surface -- and
    // a root PTY on every machine -- to anyone who can reach the port.
    auth_is_configured(cfg.oidc.is_some(), repo::local_admin_exists(&pool).await?)?;

    // Load (or generate + persist) the internal CA, then issue the control
    // plane's own TLS leaf for the agent gRPC surface (Spine, build slice #1).
    let field_cipher = crypto::FieldCipher::from_b64_key(&cfg.field_key_b64)?;
    let ca = Arc::new(ca::CertAuthority::load_or_init(&pool, &field_cipher).await?);
    let server_identity = ca.issue_server_cert(&cfg.agent_sans)?;

    let hub = Arc::new(hub::Hub::new());
    let agent_svc = grpc::AgentSvc::new(ca, pool.clone(), hub.clone());

    // Serve the browser HTTP surface, the agent gRPC surface, the offline +
    // expired-session sweeper, and the hourly metrics-retention prune
    // concurrently.
    tokio::try_join!(
        http::serve(&cfg, pool.clone(), hub.clone()),
        grpc::serve(&cfg, agent_svc, server_identity),
        jobs::run(pool.clone()),
        jobs::prune_metrics(pool.clone()),
    )?;

    Ok(())
}

/// The boot rule (design §4): the invariant is "some authentication method is
/// configured", not "OIDC specifically is". Extracted to a pure function so
/// both branches are testable without a live process -- `main`'s only job is
/// to supply the two facts this decides between.
///
/// `local_admin_present` is exactly `repo::local_admin_exists`'s result;
/// that query's own correctness (does it see a freshly-inserted row, an
/// empty table) is covered by `repo::tests::local_admin_round_trips_and_rotation_updates_the_hash`.
/// What this function alone is responsible for -- and what has no other
/// coverage -- is the decision built on top of it, and the exact wording of
/// the error naming the fix.
fn auth_is_configured(oidc_present: bool, local_admin_present: bool) -> Result<()> {
    if !oidc_present && !local_admin_present {
        anyhow::bail!(
            "no authentication is configured: set the OIDC variables, or create \
             a local admin with `argus local-admin reset`"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::auth_is_configured;

    /// Design §14: "OIDC absent with a local admin present boots" and every
    /// other combination that must NOT refuse to boot. §15 calls this
    /// non-optional: it is the only thing standing between "no auth
    /// configured" and a running control plane.
    #[test]
    fn boots_whenever_at_least_one_auth_method_is_present() {
        assert!(auth_is_configured(true, false).is_ok(), "OIDC alone");
        assert!(auth_is_configured(false, true).is_ok(), "local admin alone");
        assert!(auth_is_configured(true, true).is_ok(), "both");
    }

    /// Design §14: "both absent refuses to boot and the error names the CLI
    /// command." Asserting only `is_err()` would pass even if the message
    /// became useless -- the whole point of this branch is telling the
    /// operator which command fixes it.
    #[test]
    fn refuses_to_boot_with_neither_and_names_the_fix() {
        let err = auth_is_configured(false, false).expect_err("neither must refuse to boot");
        let msg = err.to_string();
        assert!(
            msg.contains("local-admin reset"),
            "error must name the fix, got: {msg}"
        );
    }

    /// The same decision, but composed with the REAL `repo::local_admin_exists`
    /// query against a real (test) Postgres -- the exact pipeline `main` runs,
    /// not just the boolean truth table above. Proves the wiring, not only the
    /// logic: a fresh database (no local admin, no OIDC) refuses to boot
    /// naming the fix, and after `repo::upsert_local_admin` the same
    /// OIDC-absent config boots.
    #[sqlx::test]
    async fn boot_rule_composes_with_the_real_local_admin_query(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let oidc_present = false;

        let admin_present = crate::repo::local_admin_exists(&pool).await?;
        assert!(!admin_present, "fresh database starts with no local admin");
        let err = auth_is_configured(oidc_present, admin_present)
            .expect_err("neither OIDC nor a local admin must refuse to boot");
        assert!(
            err.to_string().contains("local-admin reset"),
            "error must name the fix, got: {err}"
        );

        crate::repo::upsert_local_admin(&pool, "admin", "$argon2id$placeholder").await?;

        let admin_present = crate::repo::local_admin_exists(&pool).await?;
        assert!(
            admin_present,
            "upsert must be visible to local_admin_exists"
        );
        assert!(
            auth_is_configured(oidc_present, admin_present).is_ok(),
            "OIDC absent with a local admin present must boot"
        );

        Ok(())
    }
}
