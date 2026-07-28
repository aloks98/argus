//! Server-side DB repository (PRD §5, §6.1): enrollment-token consumption,
//! machine inventory upsert, issued-cert bookkeeping, online/offline status,
//! and the audit log. Every query is compile-time-checked via
//! `sqlx::query!`/`query_as!` against the live dev Postgres (`DATABASE_URL`).

use anyhow::Result;
use rand::Rng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Outcome of atomically checking + consuming an enrollment token.
pub enum TokenCheck {
    // `grpc::enroll` no longer uses the token's name as the audit *actor*
    // (the closed `Actor` enum has no variant for an arbitrary token name),
    // but it's still the only server-verified record of which token
    // authorized the call, so `enroll` now stamps it into every
    // `agent.enroll` row's `detail` instead (`repo::audit_with_detail`).
    //
    // `display_name`/`tags` carry the identity the token was minted with
    // (Task 4's `mint_enrollment_token`) so the enroll handler can apply it
    // to the machine right after `upsert_machine` (`apply_token_identity`
    // below) -- a null/empty value here means the token never set that
    // field, not that it should clear the machine's existing one.
    Valid {
        token_name: String,
        display_name: Option<String>,
        tags: Vec<String>,
    },
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
    /// `None` = the agent never reported (stored as NULL, gates nothing).
    /// `Some(vec![])` = reported and this host has none (gates everything).
    pub capabilities: Option<Vec<String>>,
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
        RETURNING name, display_name, tags as "tags!"
        "#,
        token_hash,
    )
    .fetch_optional(executor)
    .await?;

    Ok(match row {
        Some(row) => TokenCheck::Valid {
            token_name: row.name,
            display_name: row.display_name,
            tags: row.tags,
        },
        None => TokenCheck::Invalid,
    })
}

/// 32-character alphanumeric enrollment token, generated the same way
/// `auth::password::generate_password` builds its credential (an index drawn
/// per character via `rand::rng().random_range`), but over the full
/// `[A-Za-z0-9]` alphabet rather than password.rs's ambiguous-char-excluding
/// one: an enrollment token is copy-pasted into a join command, never
/// hand-transcribed from a screen under pressure, so the 0/O/1/l/I collision
/// risk that motivates password.rs's narrower alphabet doesn't apply here.
const TOKEN_LEN: usize = 32;
const TOKEN_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn generate_token() -> String {
    let alphabet: Vec<char> = TOKEN_ALPHABET.chars().collect();
    let mut rng = rand::rng();
    (0..TOKEN_LEN)
        .map(|_| alphabet[rng.random_range(0..alphabet.len())])
        .collect()
}

/// One `enrollment_tokens` row as returned to the browser -- never the hash,
/// never the raw token (the raw token exists only in
/// `mint_enrollment_token`'s return value, once).
pub struct TokenRow {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub tags: Vec<String>,
    pub max_uses: Option<i32>,
    pub uses: i32,
    pub expires_at: Option<OffsetDateTime>,
    pub revoked: bool,
    pub created_by: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Newest-first, for the enrollment-tokens admin table.
pub async fn list_enrollment_tokens(executor: impl sqlx::PgExecutor<'_>) -> Result<Vec<TokenRow>> {
    let rows = sqlx::query_as!(
        TokenRow,
        r#"
        SELECT id, name, display_name, tags as "tags!", max_uses, uses, expires_at,
               revoked, created_by, created_at
        FROM enrollment_tokens
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(executor)
    .await?;
    Ok(rows)
}

/// Mint a new join token: generate the raw 32-char credential, store only its
/// sha256, and compute the expiry in SQL. `expires_in_hours: None` stores
/// "never" -- confirmed against the dev DB
/// (`SELECT now() + make_interval(hours => NULL)` -> `NULL`; `make_interval`
/// propagates a NULL argument straight to a NULL result), so passing the
/// `Option<i32>` through to `make_interval(hours => $6)` needs no extra
/// `CASE`.
pub async fn mint_enrollment_token(
    executor: impl sqlx::PgExecutor<'_>,
    name: &str,
    display_name: Option<&str>,
    tags: &[String],
    max_uses: Option<i32>,
    // `int4` (Postgres `make_interval`'s `hours` parameter is `int`, not
    // `bigint`) -- 8760 (the handler's upper clamp, one year) fits
    // comfortably, so this is not a real range restriction.
    expires_in_hours: Option<i32>,
    created_by: &str,
) -> Result<(TokenRow, String)> {
    let raw = generate_token();
    let token_hash = Sha256::digest(raw.as_bytes()).to_vec();
    let row = sqlx::query_as!(
        TokenRow,
        r#"
        INSERT INTO enrollment_tokens (name, token_hash, display_name, tags, max_uses, expires_at, created_by)
        VALUES ($1, $2, $3, $4, $5, now() + make_interval(hours => $6), $7)
        RETURNING id, name, display_name, tags as "tags!", max_uses, uses, expires_at, revoked, created_by, created_at
        "#,
        name,
        token_hash,
        display_name,
        tags,
        max_uses,
        expires_in_hours,
        created_by,
    )
    .fetch_one(executor)
    .await?;
    Ok((row, raw))
}

/// Look up an enrollment token's `name` by id -- used by the revoke handler
/// to fetch the name BEFORE revoking (inside the same transaction as the
/// revoke), so the `enroll_token.revoke` audit row's detail can name the
/// token without a second round trip after the mutation.
pub async fn enrollment_token_name(
    executor: impl sqlx::PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<String>> {
    let name = sqlx::query_scalar!("SELECT name FROM enrollment_tokens WHERE id = $1", id)
        .fetch_optional(executor)
        .await?;
    Ok(name)
}

/// Revoke a token by id. `true` iff a row with this id existed (revoking an
/// already-revoked token still returns `true` -- idempotent, not an error).
pub async fn revoke_enrollment_token(
    executor: impl sqlx::PgExecutor<'_>,
    id: Uuid,
) -> Result<bool> {
    let res = sqlx::query!(
        "UPDATE enrollment_tokens SET revoked = true WHERE id = $1",
        id,
    )
    .execute(executor)
    .await?;
    Ok(res.rows_affected() == 1)
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
        INSERT INTO machines (machine_id, hostname, os, kernel, arch, primary_ip, agent_version, capabilities)
        VALUES ($1, $2, $3, $4, $5, $6::text::inet, $7, $8)
        ON CONFLICT (machine_id) DO UPDATE SET
            hostname      = EXCLUDED.hostname,
            os            = EXCLUDED.os,
            kernel        = EXCLUDED.kernel,
            arch          = EXCLUDED.arch,
            primary_ip    = EXCLUDED.primary_ip,
            agent_version = EXCLUDED.agent_version,
            capabilities  = coalesce(EXCLUDED.capabilities, machines.capabilities),
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
        info.capabilities.as_deref(),
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
            capabilities  = coalesce($8, machines.capabilities),
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
        info.capabilities.as_deref(),
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// Apply a token's identity fields to the machine it just enrolled — ONLY
/// where the token actually set them (design "Enrollment flow"): a null
/// display_name / empty tags on the token leaves the machine's existing
/// values untouched, which is what makes re-enrollment after CA rotation
/// identity-preserving by default.
pub async fn apply_token_identity(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    display_name: Option<&str>,
    tags: &[String],
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE machines SET
            display_name = coalesce($2, display_name),
            tags         = CASE WHEN cardinality($3::text[]) > 0 THEN $3 ELSE tags END,
            updated_at   = now()
        WHERE id = $1
        "#,
        machine_id,
        display_name,
        tags,
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

/// Record that a machine is alive as of now: stamp `last_seen_at` *and*
/// (re)assert `status = 'online'`. Called both at session establishment and on
/// every liveness-bearing frame thereafter.
///
/// Re-asserting the status on every such frame is load-bearing, not redundant
/// with the establishment call. `mark_stale_offline` flips any machine whose
/// `last_seen_at` falls behind the cutoff, including one whose session is still
/// up and merely stalled — a slow host, a paused VM, a network hiccup. If a
/// heartbeat only stamped the timestamp, that machine could never come back:
/// heartbeats would resume and `last_seen_at` would advance while the status
/// stayed `offline` forever, since the only other writer of `online` is a
/// brand-new session. The observable symptom is a machine whose `last_seen_at`
/// ticks up every heartbeat interval while the fleet page still shows it
/// offline.
///
/// A frame arriving on an authenticated session is itself the proof of life, so
/// it is what has to restore the status. Which frames count as proof is decided
/// by the caller (`grpc::handle_frame`) — notably log chunks do not.
pub async fn mark_online(pool: &PgPool, machine_id: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE machines SET status = 'online', last_seen_at = now(), updated_at = now() WHERE id = $1",
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

/// Delete `metrics` rows older than `older_than` (the hourly retention prune;
/// PRD's 48h retention window). Mirrors `mark_stale_offline`: the cutoff is
/// computed in Rust and bound as a plain `timestamptz` rather than binding a
/// Postgres interval. Returns the number of rows deleted.
pub async fn prune_metrics(
    exec: impl sqlx::PgExecutor<'_>,
    older_than: std::time::Duration,
) -> Result<u64> {
    let cutoff = OffsetDateTime::now_utc() - older_than;

    let result = sqlx::query!("DELETE FROM metrics WHERE ts < $1", cutoff)
        .execute(exec)
        .await?;

    Ok(result.rows_affected())
}

/// Append an audit log entry. Every verb goes through this from the start
/// (CLAUDE.md: "a verb without an audit_log write is incomplete").
pub async fn audit(
    executor: impl sqlx::PgExecutor<'_>,
    actor: Actor<'_>,
    action: &str,
    machine_id: Option<Uuid>,
    result: &str,
) -> Result<()> {
    let actor = actor.as_str();
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, result) VALUES ($1, $2, $3, $4)",
        actor.as_ref(),
        action,
        machine_id,
        result,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// Like `audit`, but also stamps a `detail` payload. For `agent.enroll`: the
/// row's `actor` alone can't carry "which enrollment token authorized this"
/// -- before a machine exists there is no principal but `Actor::System`, and
/// even once one does, `agent_id` is a claim the caller made via
/// `info.machine_id`, not a cert-verified fact (see
/// `update_machine_inventory`'s doc comment). The token hash check IS a
/// server-verified fact, and unlike the actor column it survives being
/// represented in a closed enum, so it goes in `detail` instead:
/// `{"enrollment_token": <name>}`. Without this, "which join token was used
/// to enroll/re-enroll machine X" becomes unanswerable the moment the row is
/// written -- `enrollment_tokens` only tracks a bare `uses` counter, with no
/// per-use history to fall back on.
pub async fn audit_with_detail(
    executor: impl sqlx::PgExecutor<'_>,
    actor: Actor<'_>,
    action: &str,
    machine_id: Option<Uuid>,
    result: &str,
    detail: serde_json::Value,
) -> Result<()> {
    let actor = actor.as_str();
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, result, detail)
         VALUES ($1, $2, $3, $4, $5)",
        actor.as_ref(),
        action,
        machine_id,
        result,
        detail,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// Audit a dispatched verb: like `audit`, but also records the `target_ref`
/// (container id / unit name) and the `command_id` correlating this row to the
/// gRPC `Command` and its eventual `CommandResult` (see `update_command_result`).
pub async fn audit_command(
    executor: impl sqlx::PgExecutor<'_>,
    actor: Actor<'_>,
    action: &str,
    machine_id: Option<Uuid>,
    target_ref: &str,
    command_id: Uuid,
    result: &str,
) -> Result<()> {
    let actor = actor.as_str();
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
         VALUES ($1, $2, $3, $4, $5, $6)",
        actor.as_ref(),
        action,
        machine_id,
        target_ref,
        command_id,
        result,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// Update the audit row for a dispatched command with its final result
/// (`ok`/`error`), matched by the correlated `command_id`. A no-op if no row
/// carries that command_id.
pub async fn update_command_result(
    executor: impl sqlx::PgExecutor<'_>,
    command_id: Uuid,
    machine_id: Uuid,
    result: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE audit_log SET result = $3 WHERE command_id = $1 AND machine_id = $2",
        command_id,
        machine_id,
        result,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// One heartbeat's worth of host metrics (mirrors the agent's metrics-sample
/// proto message). Persisted as-is into `metrics`; `ts` is stamped by
/// Postgres (`now()`) at insert time, not carried from the agent.
pub struct MetricsSampleRow {
    pub cpu_pct: f32,
    pub mem_used: i64,
    pub mem_total: i64,
    pub swap_used: i64,
    pub swap_total: i64,
    pub load1: f32,
    pub load5: f32,
    pub load15: f32,
    pub disk_used: i64,
    pub disk_total: i64,
    pub net_rx_bytes: i64,
    pub net_tx_bytes: i64,
    pub uptime_secs: i64,
}

/// Append one metrics sample for `machine_id`. `ts` is always `now()` at
/// insert time -- the session handler calls this once per heartbeat, so
/// there is no client-supplied timestamp to trust or distrust.
pub async fn insert_metrics(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    s: &MetricsSampleRow,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO metrics (machine_id, ts, cpu_pct, mem_used, mem_total, swap_used,
            swap_total, load1, load5, load15, disk_used, disk_total, net_rx_bytes,
            net_tx_bytes, uptime_secs)
        VALUES ($1, now(), $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        "#,
        machine_id,
        s.cpu_pct,
        s.mem_used,
        s.mem_total,
        s.swap_used,
        s.swap_total,
        s.load1,
        s.load5,
        s.load15,
        s.disk_used,
        s.disk_total,
        s.net_rx_bytes,
        s.net_tx_bytes,
        s.uptime_secs,
    )
    .execute(executor)
    .await?;

    Ok(())
}

/// One point of a fleet-grid sparkline row.
pub struct SparkRow {
    pub machine_id: Uuid,
    pub cpu_pct: Option<f32>,
    pub mem_pct: Option<f64>,
}

/// The last `per_machine` metrics samples for EVERY machine that has any,
/// ordered `(machine_id, ts ASC)` -- backs the fleet-grid sparklines, which
/// need a short recent-history strip per row without a per-machine round trip.
pub async fn recent_series_all(
    executor: impl sqlx::PgExecutor<'_>,
    per_machine: i64,
) -> Result<Vec<SparkRow>> {
    let rows = sqlx::query_as!(
        SparkRow,
        r#"
        SELECT machine_id, cpu_pct,
               (100.0 * mem_used / NULLIF(mem_total, 0))::float8 AS "mem_pct?"
        FROM (
            SELECT machine_id, ts, cpu_pct, mem_used, mem_total,
                   row_number() OVER (PARTITION BY machine_id ORDER BY ts DESC) AS rn
            FROM metrics
        ) t
        WHERE rn <= $1
        ORDER BY machine_id, ts ASC
        "#,
        per_machine,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows)
}

/// One sample of a machine's metrics history (detail-page charts). All
/// numeric fields are nullable in `metrics`, so a sample missing a given
/// reading (older agent, unreadable sysinfo field) still round-trips.
pub struct MetricPoint {
    pub ts: OffsetDateTime,
    pub cpu_pct: Option<f32>,
    pub mem_used: Option<i64>,
    pub mem_total: Option<i64>,
    pub swap_used: Option<i64>,
    pub swap_total: Option<i64>,
    pub load1: Option<f32>,
    pub disk_used: Option<i64>,
    pub disk_total: Option<i64>,
    pub net_rx_bytes: Option<i64>,
    pub net_tx_bytes: Option<i64>,
}

/// A machine's metrics history since `since`, ascending -- backs the
/// detail-page charts.
pub async fn metrics_history(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    since: OffsetDateTime,
) -> Result<Vec<MetricPoint>> {
    let rows = sqlx::query_as!(
        MetricPoint,
        r#"
        SELECT ts, cpu_pct, mem_used, mem_total, swap_used, swap_total, load1,
               disk_used, disk_total, net_rx_bytes, net_tx_bytes
        FROM metrics
        WHERE machine_id = $1 AND ts >= $2
        ORDER BY ts ASC
        "#,
        machine_id,
        since,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows)
}

/// The machine-detail page's inventory panel: the full `machines` row, with
/// `primary_ip` rendered as text via `host()`, matching the fleet query's
/// idiom for the same column.
pub struct MachineDetail {
    pub id: Uuid,
    pub machine_id: String,
    pub hostname: String,
    pub display_name: Option<String>,
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub primary_ip: Option<String>,
    pub agent_version: Option<String>,
    pub status: String,
    pub last_seen_at: Option<OffsetDateTime>,
    pub enrolled_at: OffsetDateTime,
    pub tags: Vec<String>,
    pub notes: Option<String>,
    /// `None` = never reported; the client must gate nothing in that case.
    pub capabilities: Option<Vec<String>>,
}

/// Look up one machine's full inventory row for the detail page, or `None`
/// if `id` doesn't match any machine.
pub async fn machine_detail(
    executor: impl sqlx::PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<MachineDetail>> {
    let row = sqlx::query_as!(
        MachineDetail,
        r#"
        SELECT id, machine_id, hostname, display_name, os, kernel, arch,
               host(primary_ip) as "primary_ip?", agent_version, status,
               last_seen_at, enrolled_at, tags, notes, capabilities
        FROM machines
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(executor)
    .await?;

    Ok(row)
}

/// Partial identity update. Each field is guarded by its own `apply` flag so
/// one static, compile-time-checked query covers every PATCH combination --
/// no dynamic SQL. Returns false when the machine id does not exist.
pub async fn update_machine_identity(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    display_name: Option<Option<&str>>,
    notes: Option<Option<&str>>,
    tags: Option<&[String]>,
) -> Result<bool> {
    let res = sqlx::query!(
        r#"
        UPDATE machines SET
            display_name = CASE WHEN $2 THEN $3::text ELSE display_name END,
            notes        = CASE WHEN $4 THEN $5::text ELSE notes END,
            tags         = CASE WHEN $6 THEN $7::text[] ELSE tags END,
            updated_at   = now()
        WHERE id = $1
        "#,
        machine_id,
        display_name.is_some(),
        display_name.flatten(),
        notes.is_some(),
        notes.flatten(),
        tags.is_some(),
        tags.unwrap_or(&[]),
    )
    .execute(executor)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// An authenticated human. Minted only by the auth middleware, which is what
/// makes `Actor::User` un-forgeable by a handler.
#[derive(Clone, Debug)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

impl Identity {
    /// Email reads better in an audit trail; `subject` is the stable identity
    /// and is always present. Both are on the session row (PRD §7).
    pub fn actor_str(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.subject)
    }
}

/// Who performed an audited action. A closed set on purpose: the audit column
/// used to be a free `&str`, and every browser-initiated verb passed the
/// literal "anonymous". With this, a browser verb can only be recorded by
/// producing an `Identity`, which only the auth middleware mints -- so
/// "forgot to wire the actor through" is a compile error rather than a
/// plausible-looking audit row.
pub enum Actor<'a> {
    User(&'a Identity),
    Agent(Uuid),
    /// No principal: e.g. an enrollment denied before any machine exists.
    System,
}

impl Actor<'_> {
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Actor::User(i) => std::borrow::Cow::Borrowed(i.actor_str()),
            Actor::Agent(id) => std::borrow::Cow::Owned(id.to_string()),
            Actor::System => std::borrow::Cow::Borrowed("system"),
        }
    }
}

pub async fn create_session(
    pool: &PgPool,
    token_hash: &[u8],
    identity: &Identity,
    expires_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO sessions (token_hash, subject, email, display_name, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
        token_hash,
        identity.subject,
        identity.email,
        identity.display_name,
        expires_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Expiry is enforced here, in SQL. Trusting the cookie's own `Max-Age` would
/// let a client that simply keeps sending the value stay authenticated forever.
pub async fn lookup_session(pool: &PgPool, token_hash: &[u8]) -> Result<Option<Identity>> {
    let row = sqlx::query!(
        "SELECT subject, email, display_name FROM sessions
         WHERE token_hash = $1 AND expires_at > now()",
        token_hash,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| Identity {
        subject: r.subject,
        email: r.email,
        display_name: r.display_name,
    }))
}

pub async fn delete_session(pool: &PgPool, token_hash: &[u8]) -> Result<()> {
    sqlx::query!("DELETE FROM sessions WHERE token_hash = $1", token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Hygiene, not enforcement: `lookup_session` already filters on
/// `expires_at > now()`, so an unswept row is never usable. Called from the
/// sweeper tick in `jobs::run`, after `mark_stale_offline`.
pub async fn delete_expired_sessions(pool: &PgPool) -> Result<u64> {
    let r = sqlx::query!("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

// The local-admin break-glass credential (design §6). `local_admin_exists` is
// wired to the boot rule (`main.rs`); `upsert_local_admin` is wired to the CLI
// (`argus local-admin reset`, via `auth::local::reset_local_admin`);
// `get_local_admin`/`LocalAdmin`/`touch_local_admin_login` are wired to
// `POST /auth/local` (`auth::local::login`).
pub struct LocalAdmin {
    pub username: String,
    pub password_hash: String,
}

pub async fn get_local_admin(pool: &PgPool) -> Result<Option<LocalAdmin>> {
    let row = sqlx::query!("SELECT username, password_hash FROM local_admin WHERE id = true")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| LocalAdmin {
        username: r.username,
        password_hash: r.password_hash,
    }))
}

/// Create or rotate. `ON CONFLICT` on the single-row primary key makes this an
/// in-place rotation rather than an accumulation of credentials.
pub async fn upsert_local_admin(pool: &PgPool, username: &str, password_hash: &str) -> Result<()> {
    sqlx::query!(
        "INSERT INTO local_admin (id, username, password_hash) VALUES (true, $1, $2)
         ON CONFLICT (id) DO UPDATE SET username = $1, password_hash = $2, updated_at = now()",
        username,
        password_hash,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Used by the boot rule: the control plane may start without OIDC config only
/// if this returns true.
pub async fn local_admin_exists(pool: &PgPool) -> Result<bool> {
    let n = sqlx::query_scalar!("SELECT count(*) FROM local_admin")
        .fetch_one(pool)
        .await?
        .unwrap_or(0);
    Ok(n > 0)
}

pub async fn touch_local_admin_login(pool: &PgPool) -> Result<()> {
    sqlx::query!("UPDATE local_admin SET last_login_at = now() WHERE id = true")
        .execute(pool)
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
            TokenCheck::Valid { token_name, .. } => assert_eq!(token_name, "test-token-name"),
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

    /// Minimal `AgentInfoRow` for tests that only care about machine identity
    /// (machine_id/hostname), not the full inventory snapshot exercised by
    /// `upsert_machine_is_idempotent_by_machine_id_and_updates_inventory` below.
    fn test_agent_info(machine_id: &str) -> AgentInfoRow {
        AgentInfoRow {
            machine_id: machine_id.into(),
            hostname: machine_id.into(),
            os: None,
            kernel: None,
            arch: None,
            primary_ip: None,
            agent_version: None,
            capabilities: None,
        }
    }

    #[sqlx::test]
    async fn token_identity_applies_on_enroll_and_preserves_on_reenroll(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // A token WITH identity: applies both fields.
        sqlx::query!(
            r#"INSERT INTO enrollment_tokens (name, token_hash, display_name, tags)
               VALUES ('t1', sha256('raw1'::bytea), 'Media box', '{media,infra}')"#
        )
        .execute(&pool)
        .await?;
        let TokenCheck::Valid {
            display_name, tags, ..
        } = consume_enrollment_token(&pool, "raw1").await?
        else {
            panic!("token should be valid")
        };
        let info = test_agent_info("m-ident");
        let id = upsert_machine(&pool, &info).await?;
        apply_token_identity(&pool, id, display_name.as_deref(), &tags).await?;
        let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.display_name.as_deref(), Some("Media box"));
        assert_eq!(row.tags, vec!["media", "infra"]);

        // Re-enroll with a BARE token (no identity): everything preserved.
        sqlx::query!(
            r#"INSERT INTO enrollment_tokens (name, token_hash) VALUES ('t2', sha256('raw2'::bytea))"#
        )
        .execute(&pool)
        .await?;
        let TokenCheck::Valid {
            display_name, tags, ..
        } = consume_enrollment_token(&pool, "raw2").await?
        else {
            panic!("token should be valid")
        };
        let id2 = upsert_machine(&pool, &info).await?; // same machine_id => same row
        assert_eq!(id, id2);
        apply_token_identity(&pool, id2, display_name.as_deref(), &tags).await?;
        let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            row.display_name.as_deref(),
            Some("Media box"),
            "bare re-enroll must not clear the name"
        );
        assert_eq!(
            row.tags,
            vec!["media", "infra"],
            "bare re-enroll must not clear tags"
        );
        Ok(())
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
            capabilities: None,
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
                capabilities: None,
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
                capabilities: None,
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

        mark_online(&pool, machine_row_id)
            .await
            .expect("heartbeat bump");
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

        audit(&pool, Actor::System, &action, None, "ok")
            .await
            .expect("write audit row");

        let row = sqlx::query!(
            "SELECT actor, result, machine_id FROM audit_log WHERE action = $1",
            action,
        )
        .fetch_one(&pool)
        .await
        .expect("read audit row");

        assert_eq!(row.actor, "system");
        assert_eq!(row.result.as_deref(), Some("ok"));
        assert!(row.machine_id.is_none());

        sqlx::query!("DELETE FROM audit_log WHERE action = $1", action)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    /// Seed a minimal `machines` row for the metrics tests below, returning
    /// its `id`. These run against `#[sqlx::test]`'s fresh, auto-migrated
    /// per-test database, so unlike the tests above there is no shared-DB
    /// cleanup to do.
    async fn seed_machine(pool: &PgPool, hostname: &str) -> Uuid {
        let machine_id_str = format!("test-metrics-machine-{}", Uuid::new_v4());
        sqlx::query!(
            "INSERT INTO machines (machine_id, hostname) VALUES ($1, $2) RETURNING id",
            machine_id_str,
            hostname,
        )
        .fetch_one(pool)
        .await
        .expect("seed machine")
        .id
    }

    /// Regression: a machine the sweeper flipped while its session was merely
    /// stalled must return to `online` on its next heartbeat.
    ///
    /// This failed before `mark_online` replaced the stamp-only heartbeat
    /// write. The symptom was subtle because the machine looked half-alive:
    /// `last_seen_at` advanced every heartbeat interval while the fleet page
    /// showed it offline indefinitely, since the only writer of `online` was a
    /// brand-new session and the existing session never dropped.
    ///
    /// `#[sqlx::test]` rather than the shared-DB style above because
    /// `mark_stale_offline` sweeps every row in the database, so it needs a
    /// database of its own to make an exact count meaningful.
    #[sqlx::test]
    async fn heartbeat_after_sweep_restores_online(pool: PgPool) -> anyhow::Result<()> {
        let id = seed_machine(&pool, "stalled-host").await;

        // The precondition — an online machine whose heartbeats then stalled —
        // is set up in raw SQL rather than by calling `mark_online`. Driving
        // the setup through the function under test lets a regression in it
        // hide: with the status write removed, the machine never reaches
        // `online`, the sweep below flips nothing, and the test fails at the
        // precondition instead of at the assertion that names the bug.
        //
        // Backdating past the cutoff (rather than sweeping with a zero one)
        // states the actual scenario — heartbeats stalled longer than the 45s
        // the sweeper allows — and keeps the test off the razor's edge between
        // the Rust-side cutoff and Postgres's `now()`.
        sqlx::query!(
            "UPDATE machines SET status = 'online', last_seen_at = now() - interval '5 minutes' WHERE id = $1",
            id,
        )
        .execute(&pool)
        .await?;

        let flipped = mark_stale_offline(&pool, std::time::Duration::from_secs(45)).await?;
        assert_eq!(flipped, 1, "the stalled machine should have been swept");
        let status = sqlx::query!("SELECT status FROM machines WHERE id = $1", id)
            .fetch_one(&pool)
            .await?
            .status;
        assert_eq!(status, "offline", "precondition: the sweep marked it down");

        // The session was never lost, so no new session (and no `Hello`) will
        // arrive — only the next heartbeat on the existing one.
        mark_online(&pool, id).await?;

        let row = sqlx::query!(
            "SELECT status, last_seen_at FROM machines WHERE id = $1",
            id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.status, "online",
            "a heartbeat must bring a swept-but-live machine back online"
        );
        assert!(row.last_seen_at.is_some(), "and re-stamp last_seen_at");
        Ok(())
    }

    /// A fully-populated `MetricsSampleRow` with `cpu_pct` set to `cpu_pct`
    /// so tests can distinguish samples without repeating every other field.
    fn sample_row(cpu_pct: f32) -> MetricsSampleRow {
        MetricsSampleRow {
            cpu_pct,
            mem_used: 1_000,
            mem_total: 4_000,
            swap_used: 0,
            swap_total: 2_000,
            load1: 0.1,
            load5: 0.2,
            load15: 0.3,
            disk_used: 10_000,
            disk_total: 100_000,
            net_rx_bytes: 111,
            net_tx_bytes: 222,
            uptime_secs: 3_600,
        }
    }

    #[sqlx::test]
    async fn insert_metrics_then_history_returns_it(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = seed_machine(&pool, "metrics-host-1").await;

        let sample = sample_row(12.5);
        insert_metrics(&pool, machine_id, &sample).await?;
        insert_metrics(&pool, machine_id, &sample).await?;

        let since = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let history = metrics_history(&pool, machine_id, since).await?;

        assert_eq!(history.len(), 2, "expected both inserted samples back");
        assert!(
            history[0].ts <= history[1].ts,
            "history must be ascending by ts"
        );
        assert_eq!(history[0].cpu_pct, Some(12.5));
        assert_eq!(history[1].cpu_pct, Some(12.5));

        Ok(())
    }

    #[sqlx::test]
    async fn recent_series_all_limits_per_machine(pool: PgPool) -> anyhow::Result<()> {
        let machine_a = seed_machine(&pool, "spark-host-a").await;
        let machine_b = seed_machine(&pool, "spark-host-b").await;

        for _ in 0..30 {
            insert_metrics(&pool, machine_a, &sample_row(1.0)).await?;
            insert_metrics(&pool, machine_b, &sample_row(2.0)).await?;
        }

        let rows = recent_series_all(&pool, 20).await?;
        assert_eq!(rows.len(), 40, "expected 20 rows for each of 2 machines");

        let a_count = rows.iter().filter(|r| r.machine_id == machine_a).count();
        let b_count = rows.iter().filter(|r| r.machine_id == machine_b).count();
        assert_eq!(a_count, 20, "machine_a must be capped at per_machine=20");
        assert_eq!(b_count, 20, "machine_b must be capped at per_machine=20");

        Ok(())
    }

    #[sqlx::test]
    async fn machine_detail_returns_row_or_none(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = seed_machine(&pool, "detail-host").await;

        let detail = machine_detail(&pool, machine_id)
            .await?
            .expect("expected Some for a seeded machine");
        assert_eq!(detail.hostname, "detail-host");
        assert_eq!(detail.id, machine_id);

        let missing = machine_detail(&pool, Uuid::new_v4()).await?;
        assert!(missing.is_none(), "expected None for an unknown id");

        Ok(())
    }

    #[sqlx::test]
    async fn audit_command_records_target_and_command_id(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = seed_machine(&pool, "audit-cmd-host").await;
        let command_id = Uuid::new_v4();

        audit_command(
            &pool,
            Actor::System,
            "container.start",
            Some(machine_id),
            "web",
            command_id,
            "dispatched",
        )
        .await?;

        let row = sqlx::query!(
            "SELECT actor, action, target_ref, command_id, result
             FROM audit_log WHERE command_id = $1",
            command_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.actor, "system");
        assert_eq!(row.action, "container.start");
        assert_eq!(row.target_ref.as_deref(), Some("web"));
        assert_eq!(row.command_id, Some(command_id));
        assert_eq!(row.result.as_deref(), Some("dispatched"));

        Ok(())
    }

    #[sqlx::test]
    async fn update_command_result_is_scoped_to_the_owning_machine(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id = seed_machine(&pool, "cmd-audit-host").await;
        let other_machine = seed_machine(&pool, "cmd-audit-other").await;
        let command_id = Uuid::new_v4();

        sqlx::query!(
            "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
             VALUES ('anonymous', 'container.restart', $1, 'web', $2, 'dispatched')",
            machine_id,
            command_id,
        )
        .execute(&pool)
        .await?;

        // A result attributed to a DIFFERENT machine must NOT touch this row.
        update_command_result(&pool, command_id, other_machine, "ok").await?;
        let row = sqlx::query!(
            "SELECT result FROM audit_log WHERE command_id = $1",
            command_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.result.as_deref(),
            Some("dispatched"),
            "wrong machine must not update the row"
        );

        // The owning machine updates it.
        update_command_result(&pool, command_id, machine_id, "ok").await?;
        let row = sqlx::query!(
            "SELECT result FROM audit_log WHERE command_id = $1",
            command_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("ok"));

        Ok(())
    }

    #[sqlx::test]
    async fn prune_metrics_deletes_old_rows(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = seed_machine(&pool, "prune-host").await;

        // insert_metrics always stamps ts = now(), so the old row needs a
        // direct query! INSERT specifying an explicit, stale ts.
        sqlx::query!(
            r#"
            INSERT INTO metrics (machine_id, ts, cpu_pct, mem_used, mem_total, swap_used,
                swap_total, load1, load5, load15, disk_used, disk_total, net_rx_bytes,
                net_tx_bytes, uptime_secs)
            VALUES ($1, now() - interval '3 days', 1.0, 1, 1, 0, 0, 0.1, 0.1, 0.1, 1, 1, 1, 1, 1)
            "#,
            machine_id,
        )
        .execute(&pool)
        .await?;

        insert_metrics(&pool, machine_id, &sample_row(2.0)).await?;

        let deleted = prune_metrics(&pool, std::time::Duration::from_secs(48 * 3600)).await?;
        assert_eq!(deleted, 1, "expected only the stale row to be deleted");

        let remaining = sqlx::query!(
            "SELECT count(*) as \"count!\" FROM metrics WHERE machine_id = $1",
            machine_id
        )
        .fetch_one(&pool)
        .await?
        .count;
        assert_eq!(remaining, 1, "expected the fresh row to remain");

        Ok(())
    }

    #[sqlx::test]
    async fn capabilities_round_trip_and_none_stays_distinct_from_empty(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // Reported set survives a round trip.
        let id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-1".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec!["systemd".into(), "journal".into()]),
            },
        )
        .await?;
        let got: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            got,
            Some(vec!["systemd".to_string(), "journal".to_string()])
        );

        // An EMPTY reported set is stored as `{}` and must NOT collapse to NULL:
        // it means "this host has none", which gates everything.
        let empty_id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-2".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec![]),
            },
        )
        .await?;
        let empty: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", empty_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            empty,
            Some(vec![]),
            "empty must stay empty, not become NULL"
        );

        // A pre-capability agent reports nothing at all -> NULL -> gate nothing.
        let null_id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-3".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;
        let null_caps: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", null_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(null_caps, None, "unreported must stay NULL");
        Ok(())
    }

    /// The property above (`capabilities_round_trip_and_none_stays_distinct_from_empty`)
    /// only ever inserts fresh rows, so it never reaches `upsert_machine`'s
    /// `ON CONFLICT` branch -- the one place `coalesce(EXCLUDED.capabilities,
    /// machines.capabilities)` actually runs. This test calls `upsert_machine`
    /// TWICE on the SAME `machine_id`: the second call (a silent, pre-capability
    /// agent reporting `None`) must not erase what the first call established.
    #[sqlx::test]
    async fn upsert_machine_on_conflict_none_does_not_erase_capabilities(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id = "caps-conflict-none".to_string();

        let id1 = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: machine_id.clone(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec!["systemd".into()]),
            },
        )
        .await?;

        // Same machine_id -> hits ON CONFLICT this time. A None report must not
        // clobber the capabilities established by the first upsert.
        let id2 = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: machine_id.clone(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;
        assert_eq!(id1, id2, "same machine_id must resolve to the same row");

        let got: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", id1)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            got,
            Some(vec!["systemd".to_string()]),
            "a silent (None) report on ON CONFLICT must not erase previously-established capabilities"
        );
        Ok(())
    }

    /// Companion to the above: an INTENTIONAL empty report (`Some(vec![])`, not
    /// `None`) on the SAME `machine_id` must still overwrite -- proving
    /// `coalesce` isn't just silently ignoring every update on conflict.
    #[sqlx::test]
    async fn upsert_machine_on_conflict_explicit_empty_overwrites_capabilities(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id = "caps-conflict-empty".to_string();

        let id1 = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: machine_id.clone(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec!["systemd".into()]),
            },
        )
        .await?;

        let id2 = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: machine_id.clone(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec![]),
            },
        )
        .await?;
        assert_eq!(id1, id2, "same machine_id must resolve to the same row");

        let got: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", id1)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            got,
            Some(vec![]),
            "an explicit empty report is not NULL and must overwrite, not be swallowed by coalesce"
        );
        Ok(())
    }

    /// `update_machine_inventory` is the Hello-path refresh, keyed by the
    /// authenticated `machines.id` (never the self-reported `machine_id`
    /// string). Nothing in the task's tests called it at all; pin both halves
    /// of the tri-state contract on it directly: a `None` report preserves
    /// what's on disk, and a subsequent explicit report overwrites it.
    #[sqlx::test]
    async fn update_machine_inventory_none_preserves_then_explicit_overwrites_capabilities(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-hello".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec!["systemd".into()]),
            },
        )
        .await?;

        // A silent (None) Hello must not erase what enrollment established.
        update_machine_inventory(
            &pool,
            id,
            &AgentInfoRow {
                machine_id: "caps-hello".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let got: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            got,
            Some(vec!["systemd".to_string()]),
            "update_machine_inventory must not erase capabilities on a None report"
        );

        // A subsequent Hello with an explicit new set must overwrite.
        update_machine_inventory(
            &pool,
            id,
            &AgentInfoRow {
                machine_id: "caps-hello".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec!["systemd".into(), "docker".into()]),
            },
        )
        .await?;

        let got: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            got,
            Some(vec!["systemd".to_string(), "docker".to_string()]),
            "update_machine_inventory must apply an explicit new report"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn session_round_trips_and_expiry_is_enforced_in_sql(pool: PgPool) -> anyhow::Result<()> {
        let id = Identity {
            subject: "sub-1".into(),
            email: Some("a@example.com".into()),
            display_name: Some("A".into()),
        };
        let hash = vec![1u8; 32];
        let future = OffsetDateTime::now_utc() + time::Duration::hours(1);
        create_session(&pool, &hash, &id, future).await?;

        let found = lookup_session(&pool, &hash).await?.expect("session exists");
        assert_eq!(found.subject, "sub-1");
        assert_eq!(found.email.as_deref(), Some("a@example.com"));

        // An unknown token must not resolve to anyone.
        assert!(lookup_session(&pool, &[9u8; 32]).await?.is_none());

        // Expiry is enforced by the QUERY, not by cookie age -- a client that
        // keeps presenting an old cookie must still be rejected.
        sqlx::query!(
            "UPDATE sessions SET expires_at = now() - interval '1 second' WHERE token_hash = $1",
            &hash
        )
        .execute(&pool)
        .await?;
        assert!(lookup_session(&pool, &hash).await?.is_none());
        Ok(())
    }

    #[sqlx::test]
    async fn delete_session_revokes_immediately(pool: PgPool) -> anyhow::Result<()> {
        let id = Identity {
            subject: "s".into(),
            email: None,
            display_name: None,
        };
        let hash = vec![2u8; 32];
        create_session(
            &pool,
            &hash,
            &id,
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await?;
        delete_session(&pool, &hash).await?;
        assert!(lookup_session(&pool, &hash).await?.is_none());
        Ok(())
    }

    #[sqlx::test]
    async fn sweeper_deletes_only_expired_sessions(pool: PgPool) -> anyhow::Result<()> {
        let id = Identity {
            subject: "s".into(),
            email: None,
            display_name: None,
        };
        let live = vec![3u8; 32];
        let dead = vec![4u8; 32];
        create_session(
            &pool,
            &live,
            &id,
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await?;
        create_session(
            &pool,
            &dead,
            &id,
            OffsetDateTime::now_utc() - time::Duration::hours(1),
        )
        .await?;

        assert_eq!(delete_expired_sessions(&pool).await?, 1);
        assert!(lookup_session(&pool, &live).await?.is_some());
        Ok(())
    }

    #[test]
    fn actor_string_prefers_email_but_falls_back_to_subject() {
        let with = Identity {
            subject: "sub".into(),
            email: Some("a@example.com".into()),
            display_name: None,
        };
        assert_eq!(with.actor_str(), "a@example.com");
        let without = Identity {
            subject: "sub".into(),
            email: None,
            display_name: None,
        };
        assert_eq!(without.actor_str(), "sub");
    }

    #[test]
    fn actor_renders_each_principal_kind() {
        let id = Identity {
            subject: "sub".into(),
            email: Some("a@example.com".into()),
            display_name: None,
        };
        assert_eq!(Actor::User(&id).as_str(), "a@example.com");
        let m = Uuid::nil();
        assert_eq!(Actor::Agent(m).as_str(), m.to_string());
        assert_eq!(Actor::System.as_str(), "system");
    }

    #[sqlx::test]
    async fn local_admin_round_trips_and_rotation_updates_the_hash(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        assert!(!local_admin_exists(&pool).await?, "starts absent");
        assert!(get_local_admin(&pool).await?.is_none());

        upsert_local_admin(&pool, "admin", "$argon2id$first").await?;
        assert!(local_admin_exists(&pool).await?);
        let a = get_local_admin(&pool).await?.expect("row");
        assert_eq!(a.username, "admin");
        assert_eq!(a.password_hash, "$argon2id$first");

        let updated_at_after_first = sqlx::query_scalar!("SELECT updated_at FROM local_admin")
            .fetch_one(&pool)
            .await?;

        // Rotation replaces the hash in place rather than adding a row, and
        // design §14 requires `updated_at` to advance along with it -- not
        // just the hash.
        upsert_local_admin(&pool, "admin", "$argon2id$second").await?;
        let b = get_local_admin(&pool).await?.expect("row");
        assert_eq!(b.password_hash, "$argon2id$second");

        let updated_at_after_second = sqlx::query_scalar!("SELECT updated_at FROM local_admin")
            .fetch_one(&pool)
            .await?;
        assert!(
            updated_at_after_second > updated_at_after_first,
            "rotation must bump updated_at, not just the hash"
        );

        let count = sqlx::query_scalar!("SELECT count(*) FROM local_admin")
            .fetch_one(&pool)
            .await?
            .unwrap_or(0);
        assert_eq!(count, 1, "rotation must not create a second row");
        Ok(())
    }

    #[sqlx::test]
    async fn a_second_local_admin_row_is_rejected_by_the_schema(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        upsert_local_admin(&pool, "admin", "$argon2id$x").await?;

        // Bypass the repo helper to prove the SCHEMA enforces single-row, not
        // just our upsert. Two different constraints are in play, and both
        // need their own row to be proven:

        // 1. `id = true` collides with the existing row's PRIMARY KEY. This
        //    alone would be rejected even if `local_admin_single_row` (the
        //    `check (id)` constraint) did not exist.
        let same_id = sqlx::query!(
            "INSERT INTO local_admin (id, username, password_hash) VALUES (true, 'other', 'y')"
        )
        .execute(&pool)
        .await;
        assert!(
            same_id.is_err(),
            "a second id = true row must collide on the primary key"
        );

        // 2. `id = false` is a DISTINCT primary key value, so the PK alone
        //    permits it -- only `local_admin_single_row`'s `check (id)`
        //    blocks it. This is the constraint's actual job: without it, a
        //    row with id = false would sit in the table, invisible to
        //    `get_local_admin` (which filters `WHERE id = true`), while
        //    every other test in this file kept passing.
        let other_id = sqlx::query!(
            "INSERT INTO local_admin (id, username, password_hash) VALUES (false, 'other', 'y')"
        )
        .execute(&pool)
        .await;
        assert!(
            other_id.is_err(),
            "the check constraint must reject id = false"
        );

        Ok(())
    }

    #[sqlx::test]
    async fn touch_login_stamps_last_login_at(pool: PgPool) -> anyhow::Result<()> {
        upsert_local_admin(&pool, "admin", "$argon2id$x").await?;
        let before = sqlx::query_scalar!("SELECT last_login_at FROM local_admin")
            .fetch_one(&pool)
            .await?;
        assert!(before.is_none(), "not stamped until a login happens");

        touch_local_admin_login(&pool).await?;
        let after = sqlx::query_scalar!("SELECT last_login_at FROM local_admin")
            .fetch_one(&pool)
            .await?;
        assert!(after.is_some());
        Ok(())
    }
}
