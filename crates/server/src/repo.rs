//! Server-side DB repository (PRD §5, §6.1): enrollment-token consumption,
//! machine inventory upsert, issued-cert bookkeeping, online/offline status,
//! and the audit log. Every query is compile-time-checked via
//! `sqlx::query!`/`query_as!` against the live dev Postgres (`DATABASE_URL`).

use anyhow::Result;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Outcome of atomically checking + consuming an enrollment token.
pub enum TokenCheck {
    Valid { token_name: String },
    Invalid,
}

/// Inventory snapshot carried by the agent's `Hello`/`AgentInfo` (mirrors
/// `proto AgentInfo`). `machine_id` and `hostname` are `NOT NULL` in
/// `machines`; the rest are nullable there and may be unset per platform.
pub struct AgentInfoRow {
    pub machine_id: String,
    pub hostname: String,
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub primary_ip: Option<String>,
    pub agent_version: Option<String>,
}

/// Hash `token_plain` with sha256 and atomically check-and-consume the
/// matching `enrollment_tokens` row: rejects revoked/expired/uses-exhausted
/// tokens, otherwise increments `uses` and returns the token's `name`. The
/// raw token is never stored -- only its hash is ever looked up (PRD §5.2).
/// The check and the increment happen in one `UPDATE ... RETURNING` so two
/// concurrent enrollments can't both slip through on the last remaining use.
pub async fn consume_enrollment_token(
    executor: impl sqlx::PgExecutor<'_>,
    token_plain: &str,
) -> Result<TokenCheck> {
    let token_hash = Sha256::digest(token_plain.as_bytes()).to_vec();

    let row = sqlx::query!(
        r#"
        UPDATE enrollment_tokens
        SET uses = uses + 1
        WHERE token_hash = $1
          AND NOT revoked
          AND (expires_at IS NULL OR expires_at > now())
          AND (max_uses IS NULL OR uses < max_uses)
        RETURNING name
        "#,
        token_hash,
    )
    .fetch_optional(executor)
    .await?;

    Ok(match row {
        Some(row) => TokenCheck::Valid {
            token_name: row.name,
        },
        None => TokenCheck::Invalid,
    })
}

/// Insert-or-update a machine's inventory row by `machine_id` (the stable
/// `/etc/machine-id`, survives re-enrollment), returning `machines.id` (the
/// `agent_id` embedded in the issued client cert's CN).
pub async fn upsert_machine(
    executor: impl sqlx::PgExecutor<'_>,
    info: &AgentInfoRow,
) -> Result<Uuid> {
    // `primary_ip` is `inet`; casting the bound text through `::inet` avoids
    // pulling in the `ipnetwork` sqlx feature just for this one column.
    let row = sqlx::query!(
        r#"
        INSERT INTO machines (machine_id, hostname, os, kernel, arch, primary_ip, agent_version)
        VALUES ($1, $2, $3, $4, $5, $6::text::inet, $7)
        ON CONFLICT (machine_id) DO UPDATE SET
            hostname      = EXCLUDED.hostname,
            os            = EXCLUDED.os,
            kernel        = EXCLUDED.kernel,
            arch          = EXCLUDED.arch,
            primary_ip    = EXCLUDED.primary_ip,
            agent_version = EXCLUDED.agent_version,
            updated_at    = now()
        RETURNING id
        "#,
        info.machine_id,
        info.hostname,
        info.os,
        info.kernel,
        info.arch,
        info.primary_ip,
        info.agent_version,
    )
    .fetch_one(executor)
    .await?;

    Ok(row.id)
}

/// Refresh a machine's inventory columns, keyed by the AUTHENTICATED id (never
/// the agent's self-reported machine_id string). Does not touch machine_id,
/// status, tags, or enrolled_at.
///
/// `Session`'s `Hello` handling must call this instead of `upsert_machine`: the
/// cert-authenticated `machine_id` UUID is the only trustworthy identity for an
/// already-connected agent, whereas `info.machine_id` is a self-reported string
/// an authenticated-but-misbehaving (or misconfigured) agent could set to
/// anything -- including another machine's `machine_id`, which would let its
/// inventory silently overwrite that other machine's row via `upsert_machine`'s
/// `ON CONFLICT (machine_id)`.
pub async fn update_machine_inventory(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    info: &AgentInfoRow,
) -> Result<()> {
    // `primary_ip` is `inet`; casting the bound text through `::inet` matches
    // `upsert_machine`'s handling of the same column.
    sqlx::query!(
        r#"
        UPDATE machines SET
            hostname      = $2,
            os            = $3,
            kernel        = $4,
            arch          = $5,
            primary_ip    = $6::text::inet,
            agent_version = $7,
            updated_at    = now()
        WHERE id = $1
        "#,
        machine_id,
        info.hostname,
        info.os,
        info.kernel,
        info.arch,
        info.primary_ip,
        info.agent_version,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// Record a freshly-issued client cert against its machine (PRD §5.3).
/// `serial` is the decimal serial string from `ca::SignedCert::serial`;
/// binding it through `::numeric` avoids pulling in sqlx's `bigdecimal`
/// feature just to accept a `numeric` column.
pub async fn insert_agent_cert(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    serial: &str,
    fingerprint: &str,
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO agent_certs (machine_id, serial, fingerprint, not_before, not_after)
        VALUES ($1, $2::text::numeric, $3, $4, $5)
        "#,
        machine_id,
        serial,
        fingerprint,
        not_before,
        not_after,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// Look up the machine identified by a client cert's `fingerprint`, if it
/// matches a non-revoked, unexpired `agent_certs` row -- the mTLS identity
/// check every agent gRPC call rides on (PRD §5).
pub async fn cert_is_active(pool: &PgPool, fingerprint: &str) -> Result<Option<Uuid>> {
    let row = sqlx::query!(
        r#"
        SELECT machine_id
        FROM agent_certs
        WHERE fingerprint = $1
          AND NOT revoked
          AND not_after > now()
        "#,
        fingerprint,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.machine_id))
}

/// Flip a machine to `online` (session established) and stamp `last_seen_at`.
pub async fn mark_online(pool: &PgPool, machine_id: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE machines SET status = 'online', last_seen_at = now(), updated_at = now() WHERE id = $1",
        machine_id,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Bump `last_seen_at` on each heartbeat without otherwise touching `status`.
pub async fn touch_last_seen(pool: &PgPool, machine_id: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE machines SET last_seen_at = now(), updated_at = now() WHERE id = $1",
        machine_id,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Flip any `online` machine not seen within `older_than` to `offline` (a
/// periodic background sweep). The cutoff is computed in Rust and bound as a
/// plain `timestamptz`, rather than binding a Postgres interval. Returns the
/// number of rows flipped.
pub async fn mark_stale_offline(pool: &PgPool, older_than: std::time::Duration) -> Result<u64> {
    let cutoff = OffsetDateTime::now_utc() - older_than;

    let result = sqlx::query!(
        r#"
        UPDATE machines
        SET status = 'offline', updated_at = now()
        WHERE status = 'online'
          AND (last_seen_at IS NULL OR last_seen_at < $1)
        "#,
        cutoff,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Append an audit log entry. Every verb goes through this from the start
/// (CLAUDE.md: "a verb without an audit_log write is incomplete").
pub async fn audit(
    executor: impl sqlx::PgExecutor<'_>,
    actor: &str,
    action: &str,
    machine_id: Option<Uuid>,
    result: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, result) VALUES ($1, $2, $3, $4)",
        actor,
        action,
        machine_id,
        result,
    )
    .execute(executor)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connect to the dev Postgres these `--ignored` tests run against.
    async fn test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        PgPool::connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL / a live Postgres; run with `-- --ignored`"]
    async fn consume_enrollment_token_enforces_uses_revoked_and_expiry() {
        let pool = test_pool().await;

        // -- valid once, then Invalid once max_uses is exhausted --
        let plain = format!("test-token-{}", Uuid::new_v4());
        let hash = Sha256::digest(plain.as_bytes()).to_vec();
        sqlx::query!(
            "INSERT INTO enrollment_tokens (name, token_hash, max_uses, uses, expires_at, revoked)
             VALUES ($1, $2, 1, 0, NULL, false)",
            "test-token-name",
            hash,
        )
        .execute(&pool)
        .await
        .expect("seed token");

        match consume_enrollment_token(&pool, &plain)
            .await
            .expect("consume")
        {
            TokenCheck::Valid { token_name } => assert_eq!(token_name, "test-token-name"),
            TokenCheck::Invalid => panic!("expected Valid on first use"),
        }
        assert!(matches!(
            consume_enrollment_token(&pool, &plain)
                .await
                .expect("consume again"),
            TokenCheck::Invalid
        ));

        sqlx::query!("DELETE FROM enrollment_tokens WHERE token_hash = $1", hash)
            .execute(&pool)
            .await
            .expect("cleanup exhausted token");

        // -- revoked --
        let plain_revoked = format!("test-token-revoked-{}", Uuid::new_v4());
        let hash_revoked = Sha256::digest(plain_revoked.as_bytes()).to_vec();
        sqlx::query!(
            "INSERT INTO enrollment_tokens (name, token_hash, max_uses, uses, expires_at, revoked)
             VALUES ($1, $2, NULL, 0, NULL, true)",
            "revoked-token",
            hash_revoked,
        )
        .execute(&pool)
        .await
        .expect("seed revoked token");

        assert!(matches!(
            consume_enrollment_token(&pool, &plain_revoked)
                .await
                .expect("consume revoked"),
            TokenCheck::Invalid
        ));

        sqlx::query!(
            "DELETE FROM enrollment_tokens WHERE token_hash = $1",
            hash_revoked
        )
        .execute(&pool)
        .await
        .expect("cleanup revoked token");

        // -- expired --
        let plain_expired = format!("test-token-expired-{}", Uuid::new_v4());
        let hash_expired = Sha256::digest(plain_expired.as_bytes()).to_vec();
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        sqlx::query!(
            "INSERT INTO enrollment_tokens (name, token_hash, max_uses, uses, expires_at, revoked)
             VALUES ($1, $2, NULL, 0, $3, false)",
            "expired-token",
            hash_expired,
            past,
        )
        .execute(&pool)
        .await
        .expect("seed expired token");

        assert!(matches!(
            consume_enrollment_token(&pool, &plain_expired)
                .await
                .expect("consume expired"),
            TokenCheck::Invalid
        ));

        sqlx::query!(
            "DELETE FROM enrollment_tokens WHERE token_hash = $1",
            hash_expired
        )
        .execute(&pool)
        .await
        .expect("cleanup expired token");
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL / a live Postgres; run with `-- --ignored`"]
    async fn upsert_machine_is_idempotent_by_machine_id_and_updates_inventory() {
        let pool = test_pool().await;
        let machine_id = format!("test-repo-machine-{}", Uuid::new_v4());

        let info_v1 = AgentInfoRow {
            machine_id: machine_id.clone(),
            hostname: "host-v1".into(),
            os: Some("Debian 12".into()),
            kernel: Some("6.1.0".into()),
            arch: Some("x86_64".into()),
            primary_ip: Some("10.0.0.5".into()),
            agent_version: Some("0.1.0".into()),
        };
        let id1 = upsert_machine(&pool, &info_v1).await.expect("first upsert");

        let info_v2 = AgentInfoRow {
            hostname: "host-v2".into(),
            ..info_v1
        };
        let id2 = upsert_machine(&pool, &info_v2)
            .await
            .expect("second upsert");

        assert_eq!(
            id1, id2,
            "same machine_id must resolve to the same machines.id"
        );

        let row = sqlx::query!("SELECT hostname FROM machines WHERE id = $1", id1)
            .fetch_one(&pool)
            .await
            .expect("read back");
        assert_eq!(row.hostname, "host-v2");

        sqlx::query!("DELETE FROM machines WHERE id = $1", id1)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL / a live Postgres; run with `-- --ignored`"]
    async fn insert_agent_cert_and_cert_is_active_respects_revocation() {
        let pool = test_pool().await;
        let machine_id_str = format!("test-repo-cert-machine-{}", Uuid::new_v4());
        let machine_row_id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: machine_id_str,
                hostname: "cert-host".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
            },
        )
        .await
        .expect("seed machine");

        let serial = rand::random::<u64>().to_string();
        let fingerprint = format!("test-fp-{}", Uuid::new_v4().simple());
        let now = OffsetDateTime::now_utc();
        insert_agent_cert(
            &pool,
            machine_row_id,
            &serial,
            &fingerprint,
            now - time::Duration::hours(1),
            now + time::Duration::days(365),
        )
        .await
        .expect("insert cert");

        let active = cert_is_active(&pool, &fingerprint)
            .await
            .expect("check active");
        assert_eq!(active, Some(machine_row_id));

        sqlx::query!(
            "UPDATE agent_certs SET revoked = true WHERE fingerprint = $1",
            fingerprint,
        )
        .execute(&pool)
        .await
        .expect("revoke");

        let revoked = cert_is_active(&pool, &fingerprint)
            .await
            .expect("check revoked");
        assert_eq!(revoked, None);

        // Cascades to agent_certs (machines.id -> agent_certs.machine_id ON DELETE CASCADE).
        sqlx::query!("DELETE FROM machines WHERE id = $1", machine_row_id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL / a live Postgres; run with `-- --ignored`"]
    async fn mark_online_then_mark_stale_offline_flips_status() {
        let pool = test_pool().await;
        let machine_id_str = format!("test-repo-status-machine-{}", Uuid::new_v4());
        let machine_row_id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: machine_id_str,
                hostname: "status-host".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
            },
        )
        .await
        .expect("seed machine");

        mark_online(&pool, machine_row_id)
            .await
            .expect("mark online");

        let row = sqlx::query!("SELECT status FROM machines WHERE id = $1", machine_row_id)
            .fetch_one(&pool)
            .await
            .expect("read status");
        assert_eq!(row.status, "online");

        touch_last_seen(&pool, machine_row_id)
            .await
            .expect("touch last seen");
        let row = sqlx::query!(
            "SELECT last_seen_at FROM machines WHERE id = $1",
            machine_row_id
        )
        .fetch_one(&pool)
        .await
        .expect("read last_seen_at");
        assert!(row.last_seen_at.is_some());

        let affected = mark_stale_offline(&pool, std::time::Duration::from_secs(0))
            .await
            .expect("sweep");
        assert!(
            affected >= 1,
            "sweep must flip at least the just-onlined machine"
        );

        let row = sqlx::query!("SELECT status FROM machines WHERE id = $1", machine_row_id)
            .fetch_one(&pool)
            .await
            .expect("read status after sweep");
        assert_eq!(row.status, "offline");

        sqlx::query!("DELETE FROM machines WHERE id = $1", machine_row_id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL / a live Postgres; run with `-- --ignored`"]
    async fn audit_inserts_a_retrievable_row() {
        let pool = test_pool().await;
        let action = format!("test.repo.audit.{}", Uuid::new_v4());

        audit(&pool, "test-actor", &action, None, "ok")
            .await
            .expect("write audit row");

        let row = sqlx::query!(
            "SELECT actor, result, machine_id FROM audit_log WHERE action = $1",
            action,
        )
        .fetch_one(&pool)
        .await
        .expect("read audit row");

        assert_eq!(row.actor, "test-actor");
        assert_eq!(row.result.as_deref(), Some("ok"));
        assert!(row.machine_id.is_none());

        sqlx::query!("DELETE FROM audit_log WHERE action = $1", action)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
}
