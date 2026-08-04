//! Argus control plane. Stateless: all state lives in Postgres (PRD §2).
//! Serves two network surfaces -- browser HTTP (behind Traefik) and agent
//! mTLS gRPC (behind a dedicated MetalLB LoadBalancer). See docs/PRD.md.

mod agent_binary;
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

use anyhow::{Context, Result};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // CLI dispatch happens before `Config::from_env` -- see
    // `run_local_admin_cli`'s doc for why. One subcommand doesn't justify an
    // argument-parsing dependency.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("local-admin") {
        return run_local_admin_cli(&args).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,argus=debug".into()),
        )
        // ANSI only when stdout is really a terminal -- same reasoning as
        // the agent's main.rs: `kubectl logs` / journald get escape codes
        // otherwise, which log viewers mangle.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    // rustls is pinned to the `ring` provider across the whole workspace.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = config::Config::from_env()?;
    tracing::info!(http_addr = %cfg.http_addr, agent_addr = %cfg.agent_addr, "starting argus control plane");

    // Boot proceeds either way (unbundled or unreadable): the agent-update
    // endpoint 503s until this is `Some`, matching the spec.
    let agent_binary = cfg
        .agent_binary_path
        .as_deref()
        .map(agent_binary::AgentBinary::load)
        .transpose()
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "agent binary configured but unreadable; self-update disabled");
            None
        })
        .map(Arc::new);

    // Readiness is gated on Postgres: connect and migrate before serving (PRD §2.5).
    let pool = db::connect(&cfg).await?;
    db::migrate(&pool).await?;
    tracing::info!("migrations applied");

    // Design §4: the invariant is "authentication is configured", not "OIDC is
    // configured". Booting with neither would serve the browser surface -- and
    // a root PTY on every machine -- to anyone who can reach the port.
    auth_is_configured(cfg.oidc.is_some(), repo::local_admin_exists(&pool).await?)?;

    let field_cipher = crypto::FieldCipher::from_b64_key(&cfg.field_key_b64)?;
    let ca = Arc::new(ca::CertAuthority::load_or_init(&pool, &field_cipher).await?);
    let server_identity = ca.issue_server_cert(&cfg.agent_sans)?;

    let hub = Arc::new(hub::Hub::new());
    let agent_svc = grpc::AgentSvc::new(ca, pool.clone(), hub.clone());

    tokio::try_join!(
        http::serve(&cfg, pool.clone(), hub.clone(), agent_binary),
        grpc::serve(&cfg, agent_svc, server_identity),
        jobs::run(pool.clone()),
        jobs::prune_retention(pool.clone()),
    )?;

    Ok(())
}

/// `argus local-admin reset [--username <name>]`; every other invocation
/// prints usage and fails.
///
/// Deliberately does NOT call `config::Config::from_env`: this is the
/// recovery path for a control plane that refuses to boot (design §4), and a
/// recovery command that needs the config that broke isn't a recovery
/// command. Reads only `ARGUS_DATABASE_URL`, via `db::connect_url`'s bare-URL form.
async fn run_local_admin_cli(args: &[String]) -> Result<()> {
    match args.get(2).map(String::as_str) {
        Some("reset") => {
            let rest = args.get(3..).unwrap_or_default();
            let username = parse_username_flag(rest).unwrap_or_else(|| "admin".to_string());

            let database_url =
                std::env::var(argus_common::env::DATABASE_URL).with_context(|| {
                    format!(
                        "missing required env var {}",
                        argus_common::env::DATABASE_URL
                    )
                })?;
            let pool = db::connect_url(&database_url).await?;
            // Idempotent/embedded (CLAUDE.md) -- free on a current DB, but on
            // a fresh one it's what stands between this working and a raw
            // "relation does not exist". The CLI path skips `main`'s boot.
            db::migrate(&pool).await?;
            let password = auth::local::reset_local_admin(&pool, &username).await?;

            // `Actor::System`: the CLI has host/database access but no
            // authenticated principal. Lives here, not in `reset_local_admin`
            // (shared with `rotate`'s `Actor::User` audit), so the username --
            // never the password -- goes in `detail`.
            //
            // Deliberately non-fatal: the reset already happened, so a failed
            // audit write must not cost the operator the password they just
            // generated.
            if let Err(e) = repo::audit_with_detail(
                &pool,
                repo::Actor::System,
                "local_admin.reset",
                None,
                "ok",
                serde_json::json!({ "username": username }),
            )
            .await
            {
                eprintln!("warning: failed to write audit log entry: {e}");
            }

            println!(
                "Local admin created.\n\n  username: {username}\n  password: {password}\n\n\
                 This password is shown ONCE and is not recoverable. Store it now.\n\
                 Run this command again to issue a new one."
            );
            Ok(())
        }
        _ => {
            eprintln!("Usage: argus local-admin reset [--username <name>]");
            std::process::exit(2);
        }
    }
}

/// Look for `--username <name>` anywhere in the trailing args, defaulting to
/// `None` (the caller falls back to `admin`) when absent or dangling.
fn parse_username_flag(rest: &[String]) -> Option<String> {
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        if arg == "--username" {
            return iter.next().cloned();
        }
    }
    None
}

/// The boot rule (design §4): "some auth method is configured", not "OIDC
/// specifically". Pure function so both branches are testable without a live
/// process. `local_admin_present` is exactly `repo::local_admin_exists`'s
/// result -- that query's own correctness is covered separately; this
/// function owns only the decision and the error's wording.
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

    /// Design §14/§15: every combination that must NOT refuse to boot -- the
    /// only thing standing between "no auth configured" and a running
    /// control plane.
    #[test]
    fn boots_whenever_at_least_one_auth_method_is_present() {
        assert!(auth_is_configured(true, false).is_ok(), "OIDC alone");
        assert!(auth_is_configured(false, true).is_ok(), "local admin alone");
        assert!(auth_is_configured(true, true).is_ok(), "both");
    }

    /// Design §14: refuses to boot, and the error names the fix. Asserting
    /// only `is_err()` would pass even if the message became useless -- the
    /// point of this branch is telling the operator what to run.
    #[test]
    fn refuses_to_boot_with_neither_and_names_the_fix() {
        let err = auth_is_configured(false, false).expect_err("neither must refuse to boot");
        let msg = err.to_string();
        assert!(
            msg.contains("local-admin reset"),
            "error must name the fix, got: {msg}"
        );
    }

    /// Composed with the REAL `repo::local_admin_exists` query -- the exact
    /// pipeline `main` runs, not just the boolean truth table above.
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
