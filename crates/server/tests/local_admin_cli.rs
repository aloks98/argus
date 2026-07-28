//! Regression coverage for the single most important property of
//! `argus local-admin reset`: it is dispatched in `main()` BEFORE
//! `config::Config::from_env` is ever called, so it works when OIDC config,
//! `ARGUS_FIELD_KEY`, and `ARGUS_PUBLIC_URL` are absent or wrong -- exactly
//! the state a control plane that refuses to boot (`auth_is_configured`) is
//! in, which is exactly when this command is needed.
//!
//! That property holds today only because the dispatch is physically the
//! first thing in `main()`. Nothing else in the suite would catch a future
//! contributor inserting a line above it (an early metrics hook, a log line,
//! a feature-flag read); this test runs the actual COMPILED BINARY as a
//! subprocess with the WHOLE environment cleared except `ARGUS_DATABASE_URL`,
//! so a regression fails in CI instead of during a real outage.
//!
//! Needs a real, already-migrated Postgres reachable via `ARGUS_DATABASE_URL`
//! (or bare `DATABASE_URL`, which is all `.forgejo/workflows/ci.yml` sets --
//! that job runs `sqlx migrate run` before `cargo test --workspace`, so the
//! `local_admin` table already exists by the time this runs). Deliberately
//! NOT `#[ignore]`d: it runs in the ordinary `cargo test --workspace` step,
//! same as every other DB-backed test in this crate.
//!
//! CAUTION: `local_admin` is a single, upsert-only row, so this test
//! necessarily overwrites it while running. Harmless in CI (the Postgres
//! container is discarded afterward); locally, the test captures whatever
//! row exists beforehand and restores it (or deletes it if none existed)
//! before making any assertion, so a real local-admin credential a developer
//! is relying on survives either way.
//!
//! Also covers the CLI's audit write (`Actor::System`, action
//! `local_admin.reset`): unlike `local_admin`, `audit_log` is an
//! append-only trail, so the row this test's reset produces is left in
//! place rather than cleaned up -- that's the same accumulation a real
//! reset in production is expected to leave behind.

use sqlx::{postgres::PgPoolOptions, Row};
use std::process::Command;

#[tokio::test]
async fn local_admin_reset_runs_with_only_the_database_url_present() {
    let database_url = std::env::var("ARGUS_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .expect("ARGUS_DATABASE_URL or DATABASE_URL must be set for this test");

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to the test database");

    // Capture whatever local_admin row exists (there may be none) so it can
    // be put back afterward -- see the CAUTION note above.
    let prior = sqlx::query("SELECT username, password_hash FROM local_admin WHERE id = true")
        .fetch_optional(&pool)
        .await
        .expect("read the prior local_admin row")
        .map(|row| {
            (
                row.get::<String, _>("username"),
                row.get::<String, _>("password_hash"),
            )
        });

    // Baseline for the audit-log assertion below: the highest existing
    // `local_admin.reset` row id, so the post-run check can look for a row
    // strictly newer than anything already there rather than merely "a row
    // exists" (which could pass even if this run wrote nothing).
    let audit_baseline_id: i64 = sqlx::query_scalar(
        "SELECT coalesce(max(id), 0) FROM audit_log WHERE action = 'local_admin.reset'",
    )
    .fetch_one(&pool)
    .await
    .expect("read the audit_log baseline");

    // The load-bearing check: the compiled binary, with the ENTIRE process
    // environment cleared except the one variable the CLI is documented to
    // need. No ARGUS_OIDC_*, no ARGUS_FIELD_KEY, no ARGUS_PUBLIC_URL --
    // `env_clear` proves this isn't passing by accident because something
    // else in the environment happened to be set.
    let output = Command::new(env!("CARGO_BIN_EXE_argus"))
        .args(["local-admin", "reset"])
        .env_clear()
        .env("ARGUS_DATABASE_URL", &database_url)
        .output()
        .expect("spawn the compiled argus binary");

    // Restore the prior row (or remove the one this test created) before any
    // assertion, so a failed assertion below still leaves the database as it
    // was found.
    match &prior {
        Some((username, password_hash)) => {
            sqlx::query(
                "UPDATE local_admin SET username = $1, password_hash = $2, updated_at = now() \
                 WHERE id = true",
            )
            .bind(username)
            .bind(password_hash)
            .execute(&pool)
            .await
            .expect("restore the prior local_admin row");
        }
        None => {
            sqlx::query("DELETE FROM local_admin WHERE id = true")
                .execute(&pool)
                .await
                .expect("remove the row this test created");
        }
    }

    assert!(
        output.status.success(),
        "`local-admin reset` must succeed with only ARGUS_DATABASE_URL present; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("username: admin"),
        "expected the default username in the output, got: {stdout}"
    );
    assert!(
        stdout.contains("password: "),
        "expected a generated password in the output, got: {stdout}"
    );

    // The reset must leave an audit trail (CLAUDE.md: "a verb without an
    // audit_log write is incomplete"), attributed to `Actor::System` (no
    // authenticated principal exists on this path) with the username -- and
    // ONLY the username, never the password -- in `detail`.
    let row = sqlx::query(
        "SELECT actor, result, detail FROM audit_log \
         WHERE action = 'local_admin.reset' AND id > $1 \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(audit_baseline_id)
    .fetch_optional(&pool)
    .await
    .expect("query the new audit_log row")
    .expect("local-admin reset must write a new local_admin.reset audit row");

    assert_eq!(row.get::<String, _>("actor"), "system");
    assert_eq!(
        row.get::<Option<String>, _>("result"),
        Some("ok".to_string())
    );
    let detail: serde_json::Value = row.get("detail");
    assert_eq!(detail, serde_json::json!({ "username": "admin" }));
}
