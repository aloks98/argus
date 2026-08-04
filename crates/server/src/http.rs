//! Browser HTTP surface (PRD §9.1), behind Traefik + cert-manager + Zitadel
//! OIDC. Serves the embedded React app (SPA fallback) plus health endpoints.

use crate::config::Config;
use crate::embed::Assets;
use crate::hub::{DispatchError, Hub, LogFilters};
use crate::repo;
use anyhow::{Context, Result};
use argus_proto::v1::Verb;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
    routing::{delete, get, post},
    Router,
};
use sqlx::PgPool;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub hub: Arc<Hub>,
    /// `None` when OIDC isn't configured (design §4) -- a valid,
    /// boot-succeeding state. `/auth/login`/`/auth/callback` degrade to 404.
    pub oidc: Option<Arc<crate::config::OidcConfig>>,
    /// The same cipher that protects `ca_material` at rest, reused to seal
    /// the OIDC flow's pre-auth cookie.
    pub cipher: Arc<crate::crypto::FieldCipher>,
    pub oidc_client: Option<Arc<crate::auth::oidc::OidcClient>>,
    /// Carried independently of `oidc` so the session cookie's `Secure`
    /// attribute can be decided with no OIDC config present -- a local login
    /// sets a cookie in exactly that state (design §4).
    pub public_url: String,
    /// One instance for the process's lifetime, shared via this `Arc` clone
    /// across every request -- a per-request limiter would limit nothing.
    pub limiter: Arc<crate::auth::ratelimit::LoginLimiter>,
    /// The agent-endpoint URLs the Enroll page interpolates into its printed
    /// block, composed from `ARGUS_AGENT_SANS` + the agent port. The server
    /// owns this composition: each hostname/IP here is a SAN on the
    /// agent-surface TLS leaf, so a value from this list passes certificate
    /// verification -- a hand-typed one only might.
    pub agent_endpoints: Vec<String>,
}

pub async fn serve(cfg: &Config, pool: PgPool, hub: Arc<Hub>) -> Result<()> {
    let oidc = cfg.oidc.clone().map(Arc::new);
    let cipher = Arc::new(
        crate::crypto::FieldCipher::from_b64_key(&cfg.field_key_b64)
            .context("building the field cipher for the OIDC flow cookie")?,
    );
    // Building this is local-only; discovery happens lazily on first login,
    // never here, so a down IdP at boot can't delay the agent gRPC surface
    // or health checks.
    let oidc_client = oidc
        .clone()
        .map(crate::auth::oidc::OidcClient::new)
        .transpose()
        .context("building the OIDC client")?
        .map(Arc::new);

    let agent_port = cfg.agent_port();
    let app = router(AppState {
        pool,
        hub,
        oidc,
        cipher,
        oidc_client,
        public_url: cfg.public_url.clone(),
        limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
        agent_endpoints: cfg
            .agent_sans
            .iter()
            .map(|san| format!("https://{san}:{agent_port}"))
            .collect(),
    });

    let listener = tokio::net::TcpListener::bind(&cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "browser HTTP surface listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the router without binding a socket, so tests can drive it directly
/// via `tower::ServiceExt::oneshot`.
fn router(state: AppState) -> Router {
    // Built as a separate Router with the auth layer applied once, so a new
    // /api route can't be added unprotected by accident.
    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/fleet", get(fleet))
        .route("/api/machines/{id}", get(machine).patch(patch_machine))
        .route("/api/machines/{id}/metrics", get(machine_metrics))
        .route("/api/machines/{id}/docker", get(machine_docker))
        .route(
            "/api/machines/{id}/docker/{container}/{action}",
            post(container_action),
        )
        .route("/api/machines/{id}/systemd", get(machine_systemd))
        .route(
            "/api/machines/{id}/units/{unit}/{action}",
            post(unit_action),
        )
        .route("/api/machines/{id}/logs/stream", get(log_stream))
        .route("/api/machines/{id}/logs/page", get(logs_page))
        .route(
            "/api/machines/{id}/terminal",
            axum::routing::any(crate::terminal::terminal_ws),
        )
        // Deliberately INSIDE this router (design §5.2) -- see `auth::local::rotate`'s doc.
        .route("/api/local-admin/rotate", post(crate::auth::local::rotate))
        .route("/api/enrollment-tokens", get(list_tokens).post(mint_token))
        .route("/api/enrollment-tokens/{id}", delete(revoke_token))
        // What the Enroll page interpolates into its printed block.
        .route("/api/enrollment-config", get(enrollment_config))
        .route("/api/audit", get(list_audit))
        // The authenticated route predates the public /ca.pem below and is
        // kept for the download button + any existing automation; both call
        // the same handler.
        .route("/api/ca.pem", get(ca_pem))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_auth,
        ));

    // Public (PRD §9.1): infra endpoints, the OIDC/local-admin login flows,
    // and the SPA bundle, which must render the sign-in view BEFORE any
    // session exists. The bundle is an empty shell; all data is behind /api.
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        // Public ON PURPOSE, outside /api (whose blanket everything-is-
        // authenticated rule stays intact): the CA cert is public by
        // definition (PRD §5) -- the Enroll RPC already hands it to any
        // join-token holder -- and a host being enrolled needs it BEFORE it
        // has any credential a browser session could supply. This is what
        // lets `curl .../ca.pem` work in cloud-init / config management.
        .route("/ca.pem", get(ca_pem))
        .route("/auth/login", get(crate::auth::oidc::login))
        .route("/auth/callback", get(crate::auth::oidc::callback))
        .route("/auth/logout", post(crate::auth::oidc::logout))
        .route("/auth/local", post(crate::auth::local::login))
        .merge(api)
        // Compression is scoped to the static bundle ON PURPOSE, not layered
        // over the whole router: compressing the SSE log stream would invite
        // proxy/browser buffering of events, and the /api JSON responses are
        // small. The ~1.5 MB JS chunk is where the bytes are.
        .fallback_service(
            axum::routing::any(static_handler)
                .layer(tower_http::compression::CompressionLayer::new()),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Current identity. Returns 401 when signed out, which is how the SPA detects
/// a signed-out state -- that 401 is a normal answer, not an error.
async fn me(crate::auth::AuthUser(id): crate::auth::AuthUser) -> impl IntoResponse {
    Json(serde_json::json!({
        "subject": id.subject,
        "email": id.email,
        "display_name": id.display_name,
    }))
}

/// Readiness gates on Postgres (PRD §2.5): the Helm chart's readiness probe
/// points here, so an unconditional "ready" would admit traffic before the
/// database (and therefore auth, enrollment, everything) is reachable.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match sqlx::query_scalar!("SELECT 1").fetch_one(&state.pool).await {
        Ok(_) => (StatusCode::OK, "ready"),
        Err(err) => {
            tracing::warn!(error = %err, "readyz: database unreachable");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable")
        }
    }
}

/// One row of the fleet page's machine table (PRD §9.1).
#[derive(serde::Serialize)]
struct FleetRow {
    id: Uuid,
    hostname: String,
    /// Operator-set name; `None` = "display the hostname". The fallback lives
    /// client-side so a hostname change keeps showing through.
    display_name: Option<String>,
    os: Option<String>,
    primary_ip: Option<String>,
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    last_seen_at: Option<OffsetDateTime>,
    tags: Vec<String>,
    /// Same tri-state as the detail payload: `None` = agent never reported =
    /// gate nothing. Carried on the fleet row for the command palette, which
    /// builds per-machine tab entries without fetching every detail page.
    capabilities: Option<Vec<String>>,
    cpu_pct: Option<f32>,
    mem_pct: Option<f64>,
    /// Count of units in the `failed` state in the machine's **last reported**
    /// Hub snapshot; `0` when nothing has been reported. Snapshots are NOT
    /// evicted on disconnect, so for an offline machine this may be stale --
    /// do not read it as a live count unless the machine's `status` is `online`.
    failed_units: usize,
    spark_cpu: Vec<f32>,
    spark_mem: Vec<f64>,
}

/// Per-machine grouping of `repo::recent_series_all`'s flat rows, in
/// `ts ASC` order (the query already orders by `(machine_id, ts ASC)`), with
/// `None` samples dropped rather than represented as gaps.
#[derive(Default)]
struct SparkSeries {
    cpu: Vec<f32>,
    mem: Vec<f64>,
}

async fn fleet(State(state): State<AppState>) -> Result<Json<Vec<FleetRow>>, StatusCode> {
    // Independent reads on the hottest endpoint (every open tab polls it
    // each 5s), so they run concurrently rather than back-to-back -- the
    // pool has ample headroom for two connections.
    let (rows, spark_rows) = tokio::join!(
        sqlx::query!(
            r#"SELECT id, hostname, display_name, os, host(primary_ip) as "primary_ip?", status,
                      last_seen_at, tags, capabilities FROM machines ORDER BY hostname"#
        )
        .fetch_all(&state.pool),
        repo::recent_series_all(&state.pool, 20),
    );
    let rows = rows.map_err(|err| {
        tracing::error!(error = %err, "failed to list fleet");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let spark_rows = spark_rows.map_err(|err| {
        tracing::error!(error = %err, "failed to load fleet sparkline series");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut series: HashMap<Uuid, SparkSeries> = HashMap::new();
    for row in spark_rows {
        let entry = series.entry(row.machine_id).or_default();
        if let Some(cpu) = row.cpu_pct {
            entry.cpu.push(cpu);
        }
        if let Some(mem) = row.mem_pct {
            entry.mem.push(mem);
        }
    }

    let out = rows
        .into_iter()
        .map(|r| {
            let s = series.remove(&r.id);
            let (spark_cpu, spark_mem) = match s {
                Some(s) => (s.cpu, s.mem),
                None => (Vec::new(), Vec::new()),
            };
            let cpu_pct = spark_cpu.last().copied();
            let mem_pct = spark_mem.last().copied();
            let failed_units = state.hub.failed_unit_count(r.id);

            FleetRow {
                id: r.id,
                hostname: r.hostname,
                display_name: r.display_name,
                os: r.os,
                primary_ip: r.primary_ip,
                status: r.status,
                last_seen_at: r.last_seen_at,
                tags: r.tags,
                capabilities: r.capabilities,
                cpu_pct,
                mem_pct,
                failed_units,
                spark_cpu,
                spark_mem,
            }
        })
        .collect();

    Ok(Json(out))
}

/// The machine-detail page's inventory panel, mirroring `repo::MachineDetail`
/// with RFC3339-serialized timestamps.
#[derive(serde::Serialize)]
struct MachineDetailDto {
    id: Uuid,
    machine_id: String,
    hostname: String,
    display_name: Option<String>,
    os: Option<String>,
    kernel: Option<String>,
    arch: Option<String>,
    primary_ip: Option<String>,
    agent_version: Option<String>,
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    last_seen_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    enrolled_at: OffsetDateTime,
    tags: Vec<String>,
    notes: Option<String>,
    /// `None` = never reported; the client must gate nothing in that case.
    capabilities: Option<Vec<String>>,
    cpu_model: Option<String>,
    cpu_cores: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    boot_time: Option<OffsetDateTime>,
    virt: Option<String>,
}

impl From<repo::MachineDetail> for MachineDetailDto {
    fn from(d: repo::MachineDetail) -> Self {
        MachineDetailDto {
            id: d.id,
            machine_id: d.machine_id,
            hostname: d.hostname,
            display_name: d.display_name,
            os: d.os,
            kernel: d.kernel,
            arch: d.arch,
            primary_ip: d.primary_ip,
            agent_version: d.agent_version,
            status: d.status,
            last_seen_at: d.last_seen_at,
            enrolled_at: d.enrolled_at,
            tags: d.tags,
            notes: d.notes,
            capabilities: d.capabilities,
            cpu_model: d.cpu_model,
            cpu_cores: d.cpu_cores,
            boot_time: d.boot_time,
            virt: d.virt,
        }
    }
}

async fn machine(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<MachineDetailDto>, StatusCode> {
    let detail = repo::machine_detail(&state.pool, id).await.map_err(|err| {
        tracing::error!(error = %err, "failed to load machine detail");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match detail {
        Some(d) => Ok(Json(d.into())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Body of `PATCH /api/machines/{id}`. The double `Option` distinguishes an
/// absent key (leave the field alone) from an explicit `null` (clear it) --
/// plain `Option<String>` cannot represent that difference after serde.
#[derive(serde::Deserialize)]
struct MachinePatch {
    #[serde(default, deserialize_with = "double_option")]
    display_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    notes: Option<Option<String>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// Shared by `MachinePatch` and the enrollment-token mint body: absent key
/// vs. explicit JSON `null` carry different meanings that plain `Option<T>`
/// can't represent after serde.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    use serde::Deserialize;
    Ok(Some(Option::<T>::deserialize(d)?))
}

/// Only fields present in the body change. Every outcome that MUTATED is
/// audited; a 400 mutates nothing and is not.
async fn patch_machine(
    State(state): State<AppState>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
    Path(id): Path<Uuid>,
    Json(patch): Json<MachinePatch>,
) -> Response {
    // Validate everything BEFORE touching the database.
    let display_name = match &patch.display_name {
        None => None,
        Some(None) => Some(None),
        Some(Some(raw)) => match crate::identity::normalize_display_name(raw) {
            Ok(v) => Some(v),
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let notes = match &patch.notes {
        None => None,
        Some(None) => Some(None),
        Some(Some(raw)) => match crate::identity::validate_notes(raw) {
            Ok(()) => Some(Some(raw.clone())),
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let tags = match &patch.tags {
        None => None,
        Some(raw) => match crate::identity::normalize_tags(raw) {
            Ok(v) => Some(v),
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let mut fields: Vec<&str> = Vec::new();
    if display_name.is_some() {
        fields.push("display_name");
    }
    if notes.is_some() {
        fields.push("notes");
    }
    if tags.is_some() {
        fields.push("tags");
    }
    if fields.is_empty() {
        return (StatusCode::BAD_REQUEST, "no fields to update").into_response();
    }

    let dn_arg: Option<Option<&str>> = display_name.as_ref().map(|o| o.as_deref());
    let notes_arg: Option<Option<&str>> = notes.as_ref().map(|o| o.as_deref());

    // Fail closed (CLAUDE.md's audit rule): the mutation and its audit row
    // share ONE transaction. Unlike `run_verb` (which audits BEFORE
    // dispatch), the 404 here can only be answered by attempting the update,
    // so the update runs first -- a failed audit write rolls it back with it.
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = %err, "failed to begin machine identity update transaction");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let updated =
        match repo::update_machine_identity(&mut *tx, id, dn_arg, notes_arg, tags.as_deref()).await
        {
            Ok(u) => u,
            Err(err) => {
                tracing::error!(error = %err, "failed to update machine identity");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    if !updated {
        // `tx` drops un-committed -- nothing to roll back, but keeps "never
        // commit without an audit row" true unconditionally, not by accident.
        return StatusCode::NOT_FOUND.into_response();
    }

    // detail = which fields changed, never the values: notes may hold anything.
    if let Err(err) = repo::audit_with_detail(
        &mut *tx,
        repo::Actor::User(&identity),
        "machine.update",
        Some(id),
        "ok",
        serde_json::json!({ "fields": fields }),
    )
    .await
    {
        tracing::error!(error = %err, "failed to audit machine.update; rolling back the update");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(err) = tx.commit().await {
        tracing::error!(error = %err, "failed to commit machine identity update");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return the refreshed detail so the client can cache-swap in one round trip.
    match repo::machine_detail(&state.pool, id).await {
        Ok(Some(d)) => Json(MachineDetailDto::from(d)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to reload machine detail");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// === Enrollment tokens (fleet-identity slice) ===

/// `token` is `None` (omitted via `skip_serializing_if`) except on the mint
/// response, where it carries the raw credential exactly once -- never
/// persisted, never returned again.
#[derive(serde::Serialize)]
struct TokenDto {
    id: Uuid,
    name: String,
    display_name: Option<String>,
    tags: Vec<String>,
    max_uses: Option<i32>,
    uses: i32,
    #[serde(with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    revoked: bool,
    created_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

impl From<repo::TokenRow> for TokenDto {
    fn from(r: repo::TokenRow) -> Self {
        TokenDto {
            id: r.id,
            name: r.name,
            display_name: r.display_name,
            tags: r.tags,
            max_uses: r.max_uses,
            uses: r.uses,
            expires_at: r.expires_at,
            revoked: r.revoked,
            created_by: r.created_by,
            created_at: r.created_at,
            token: None,
        }
    }
}

/// `GET /api/enrollment-config` -- what the Enroll page needs to print a
/// runnable block: the agent endpoints composed by the server (see
/// `AppState::agent_endpoints` for why the server, not a human, owns this
/// value). Order follows `ARGUS_AGENT_SANS`, so the operator controls which
/// endpoint the page offers first.
async fn enrollment_config(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "agent_endpoints": state.agent_endpoints }))
}

/// Newest first. Never the hash or raw token.
async fn list_tokens(State(state): State<AppState>) -> Response {
    match repo::list_enrollment_tokens(&state.pool).await {
        Ok(rows) => Json(rows.into_iter().map(TokenDto::from).collect::<Vec<_>>()).into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to list enrollment tokens");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `max_uses`/`expires_in_hours` reuse `double_option`: absent -> default (1
/// use / 24h), explicit `null` -> unlimited/never, explicit number ->
/// clamped into range. `tags` has no such tri-state (nothing to "leave
/// alone" on a create), so absent is simply "no tags".
#[derive(serde::Deserialize)]
struct MintTokenBody {
    name: String,
    display_name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "double_option")]
    max_uses: Option<Option<i32>>,
    #[serde(default, deserialize_with = "double_option")]
    expires_in_hours: Option<Option<i64>>,
}

/// The insert and its `enroll_token.create` audit row share one transaction
/// -- same fail-closed convention as `patch_machine` (see its doc): a failed
/// audit write rolls the mint back with it.
async fn mint_token(
    State(state): State<AppState>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
    Json(body): Json<MintTokenBody>,
) -> Response {
    // Validate everything BEFORE touching the database.
    let name = match crate::identity::normalize_display_name(&body.name) {
        Ok(Some(n)) => n,
        Ok(None) => return (StatusCode::BAD_REQUEST, "name must not be empty").into_response(),
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let display_name = match body.display_name.as_deref() {
        None => None,
        Some(raw) => match crate::identity::normalize_display_name(raw) {
            Ok(v) => v,
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let tags = match crate::identity::normalize_tags(&body.tags) {
        Ok(v) => v,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let max_uses = match body.max_uses {
        None => Some(1),
        Some(None) => None,
        Some(Some(n)) => Some(n.max(1)),
    };
    // Clamped in i64 first (an arbitrarily large JSON number must not
    // overflow before it's brought into range), then narrowed to i32 for
    // Postgres's `make_interval(hours => ...)`, whose parameter is `int4`.
    let expires_in_hours: Option<i32> = match body.expires_in_hours {
        None => Some(24),
        Some(None) => None,
        Some(Some(n)) => Some(n.clamp(1, 8760) as i32),
    };

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = %err, "failed to begin enrollment token mint transaction");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let (row, raw) = match repo::mint_enrollment_token(
        &mut *tx,
        &name,
        display_name.as_deref(),
        &tags,
        max_uses,
        expires_in_hours,
        identity.actor_str(),
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "failed to mint enrollment token");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if let Err(err) = repo::audit_with_detail(
        &mut *tx,
        repo::Actor::User(&identity),
        "enroll_token.create",
        None,
        "ok",
        // `id`, not just `name`: `enrollment_tokens.name` has no unique
        // constraint, so under duplicate labels the name alone can't say
        // which token this audit row is about.
        serde_json::json!({ "name": name, "id": row.id }),
    )
    .await
    {
        tracing::error!(error = %err, "failed to audit enroll_token.create; rolling back the mint");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(err) = tx.commit().await {
        tracing::error!(error = %err, "failed to commit enrollment token mint");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let mut dto = TokenDto::from(row);
    dto.token = Some(raw);
    (StatusCode::CREATED, Json(dto)).into_response()
}

/// `DELETE /api/enrollment-tokens/{id}` -- revoke a join token. 404 when
/// `id` doesn't match any token; the only way to learn that before the
/// UPDATE runs is to fetch the name up front, which conveniently is also
/// what the audit detail needs. Same share-one-transaction, fail-closed
/// convention as `mint_token` / `patch_machine`.
async fn revoke_token(
    State(state): State<AppState>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
    Path(id): Path<Uuid>,
) -> Response {
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = %err, "failed to begin enrollment token revoke transaction");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let name = match repo::enrollment_token_name(&mut *tx, id).await {
        Ok(Some(n)) => n,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to look up enrollment token before revoke");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    match repo::revoke_enrollment_token(&mut *tx, id).await {
        Ok(true) => {}
        // Only reachable via a race with a concurrent delete of the same row
        // between the lookup above and this UPDATE -- the name lookup already
        // confirmed existence, but nothing holds a lock across the gap.
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to revoke enrollment token");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    if let Err(err) = repo::audit_with_detail(
        &mut *tx,
        repo::Actor::User(&identity),
        "enroll_token.revoke",
        None,
        "ok",
        // Same reasoning as `mint_token`'s audit detail: `name` alone can't
        // disambiguate duplicate labels, so carry the id too.
        serde_json::json!({ "name": name, "id": id }),
    )
    .await
    {
        tracing::error!(error = %err, "failed to audit enroll_token.revoke; rolling back the revoke");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(err) = tx.commit().await {
        tracing::error!(error = %err, "failed to commit enrollment token revoke");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// The CA certificate, served BOTH at public `/ca.pem` (scriptable
/// bootstrap -- see the router comment) and authenticated `/api/ca.pem`
/// (the enroll page's download button; kept so existing links keep
/// working). 503 when the CA hasn't been initialized yet -- only reachable
/// in the boot window before `CertAuthority::load_or_init` has run.
async fn ca_pem(State(state): State<AppState>) -> Response {
    match sqlx::query_scalar!("SELECT cert_pem FROM ca_material WHERE id = 1")
        .fetch_optional(&state.pool)
        .await
    {
        Ok(Some(pem)) => ([(header::CONTENT_TYPE, "text/plain")], pem).into_response(),
        Ok(None) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to load CA cert");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// One sample of a machine's metrics history, mirroring `repo::MetricPoint`
/// with an RFC3339-serialized `ts`.
#[derive(serde::Serialize)]
struct MetricPointDto {
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    cpu_pct: Option<f32>,
    mem_used: Option<i64>,
    mem_total: Option<i64>,
    swap_used: Option<i64>,
    swap_total: Option<i64>,
    load1: Option<f32>,
    disk_used: Option<i64>,
    disk_total: Option<i64>,
    net_rx_bytes: Option<i64>,
    net_tx_bytes: Option<i64>,
}

impl From<repo::MetricPoint> for MetricPointDto {
    fn from(p: repo::MetricPoint) -> Self {
        MetricPointDto {
            ts: p.ts,
            cpu_pct: p.cpu_pct,
            mem_used: p.mem_used,
            mem_total: p.mem_total,
            swap_used: p.swap_used,
            swap_total: p.swap_total,
            load1: p.load1,
            disk_used: p.disk_used,
            disk_total: p.disk_total,
            net_rx_bytes: p.net_rx_bytes,
            net_tx_bytes: p.net_tx_bytes,
        }
    }
}

/// Query params for `GET /api/machines/{id}/metrics`.
#[derive(serde::Deserialize)]
struct RangeQuery {
    range: Option<String>,
}

/// `range` is one of `1h` (default), `6h`, `24h`.
async fn machine_metrics(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Vec<MetricPointDto>>, StatusCode> {
    let hours = match q.range.as_deref() {
        None | Some("1h") => 1,
        Some("6h") => 6,
        Some("24h") => 24,
        Some(_) => return Err(StatusCode::BAD_REQUEST),
    };
    let since = OffsetDateTime::now_utc() - time::Duration::hours(hours);

    let history = repo::metrics_history(&state.pool, id, since)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "failed to load machine metrics history");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(history.into_iter().map(Into::into).collect()))
}

/// Query params for `GET /api/audit`. All optional; an unknown VALUE for a
/// known param is rejected by hand below, and `deny_unknown_fields` rejects
/// an unknown param NAME (e.g. a typo'd `categry=`) via axum's `Query`
/// extractor -- both are a 400 rather than silently ignored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    category: Option<String>,
    machine: Option<String>,
    result: Option<String>,
    window: Option<String>,
    before_id: Option<i64>,
    limit: Option<i64>,
}

/// The audit action namespaces the UI can filter on. Kept in ONE place so a
/// new namespace is added here and nowhere else server-side.
const AUDIT_CATEGORIES: &[&str] = &[
    "agent",
    "auth",
    "container",
    "unit",
    "logs",
    "terminal",
    "enroll_token",
    "machine",
    "local_admin",
];

#[derive(serde::Serialize)]
struct AuditRowDto {
    id: i64,
    ts: String,
    actor: String,
    action: String,
    machine_id: Option<Uuid>,
    hostname: Option<String>,
    target_ref: Option<String>,
    result: Option<String>,
    detail: serde_json::Value,
}

#[derive(serde::Serialize)]
struct AuditPageDto {
    rows: Vec<AuditRowDto>,
    has_more: bool,
}

/// Read-only page over `audit_log`. Deliberately writes NO audit row itself:
/// reads that don't touch an agent are not audited anywhere (`logs.open` is
/// audited because it drives one).
async fn list_audit(State(state): State<AppState>, Query(q): Query<AuditQuery>) -> Response {
    let category = match q.category.as_deref() {
        None => None,
        Some(c) if AUDIT_CATEGORIES.contains(&c) => Some(c),
        Some(_) => return (StatusCode::BAD_REQUEST, "unknown category").into_response(),
    };
    let machine_id = match q.machine.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<Uuid>() {
            Ok(id) => Some(id),
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid machine id").into_response(),
        },
    };
    let result = match q.result.as_deref() {
        None => None,
        Some(r @ ("ok" | "error" | "denied")) => Some(r),
        Some(_) => return (StatusCode::BAD_REQUEST, "unknown result").into_response(),
    };
    let since = match q.window.as_deref() {
        None | Some("7d") => Some(OffsetDateTime::now_utc() - time::Duration::days(7)),
        Some("24h") => Some(OffsetDateTime::now_utc() - time::Duration::hours(24)),
        Some("30d") => Some(OffsetDateTime::now_utc() - time::Duration::days(30)),
        Some("all") => None,
        Some(_) => return (StatusCode::BAD_REQUEST, "unknown window").into_response(),
    };
    let before_id = match q.before_id {
        None => None,
        Some(id) if id >= 1 => Some(id),
        Some(_) => return (StatusCode::BAD_REQUEST, "invalid before_id").into_response(),
    };
    let limit = q.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return (StatusCode::BAD_REQUEST, "limit must be 1..=500").into_response();
    }

    let page = match repo::audit_page(
        &state.pool,
        repo::AuditFilters {
            category,
            machine_id,
            result,
            since,
            before_id,
            limit,
        },
    )
    .await
    {
        Ok(page) => page,
        Err(err) => {
            tracing::error!(error = %err, "failed to load audit page");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let rows = page
        .rows
        .into_iter()
        .map(|r| AuditRowDto {
            id: r.id,
            ts: r
                .ts
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            actor: r.actor,
            action: r.action,
            machine_id: r.machine_id,
            hostname: r.hostname,
            target_ref: r.target_ref,
            result: r.result,
            detail: r.detail,
        })
        .collect();

    Json(AuditPageDto {
        rows,
        has_more: page.has_more,
    })
    .into_response()
}

/// One container row for the detail page's container panel, mirroring the proto
/// `Container` (which isn't `Serialize`).
#[derive(serde::Serialize)]
struct ContainerDto {
    id: String,
    name: String,
    image: String,
    state: String,
    status: String,
    health: String,
}

impl From<argus_proto::v1::Container> for ContainerDto {
    fn from(c: argus_proto::v1::Container) -> Self {
        ContainerDto {
            id: c.id,
            name: c.name,
            image: c.image,
            state: c.state,
            status: c.status,
            health: c.health,
        }
    }
}

/// The machine's latest cached container list (empty if the agent hasn't
/// reported, or has no Docker daemon).
async fn machine_docker(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Json<Vec<ContainerDto>> {
    let containers = state.hub.get_docker(id);
    Json(containers.into_iter().map(ContainerDto::from).collect())
}

/// One unit row for the detail page's units panel, mirroring the proto `Unit`
/// (which isn't `Serialize`).
#[derive(serde::Serialize)]
struct UnitDto {
    name: String,
    load_state: String,
    active_state: String,
    sub_state: String,
    description: String,
}

impl From<argus_proto::v1::Unit> for UnitDto {
    fn from(u: argus_proto::v1::Unit) -> Self {
        UnitDto {
            name: u.name,
            load_state: u.load_state,
            active_state: u.active_state,
            sub_state: u.sub_state,
            description: u.description,
        }
    }
}

/// The machine's latest cached unit list (empty if the agent hasn't
/// reported, or has no systemd).
async fn machine_systemd(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Json<Vec<UnitDto>> {
    let units = state.hub.get_systemd(id);
    Json(units.into_iter().map(UnitDto::from).collect())
}

/// Hard ceiling on the backlog a client may ask the agent to render.
pub const MAX_TAIL_LINES: u32 = 1000;

/// Default backlog when the client doesn't ask for one.
const DEFAULT_TAIL_LINES: u32 = 200;

/// Query params for `GET /api/machines/{id}/logs/stream`.
#[derive(serde::Deserialize)]
struct LogStreamQuery {
    source: String,
    tail: Option<u32>,
    follow: Option<bool>,
    priority: Option<u32>,
    window: Option<String>,
}

/// Query params for `GET /api/machines/{id}/logs/page`.
#[derive(serde::Deserialize)]
struct LogPageQuery {
    source: String,
    before: Option<String>,
    limit: Option<u32>,
    priority: Option<u32>,
    window: Option<String>,
    since_ms: Option<u64>,
}

/// One page of older journal entries plus the anchor for the next page.
#[derive(serde::Serialize)]
struct LogPage {
    lines: Vec<serde_json::Value>,
    oldest_cursor: Option<String>,
    reached_start: bool,
}

/// Server-side source validation. The agent validates independently — neither
/// side trusts the other, because this value becomes a subprocess argument.
fn source_is_valid(raw: &str) -> bool {
    let Some((scheme, target)) = raw.split_once(':') else {
        return false;
    };
    if target.is_empty() || target.len() > 256 {
        return false;
    }
    match scheme {
        "journal" => target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-' | '\\')),
        "docker" => target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
        _ => false,
    }
}

/// `window` is one of `boot | 1h | 24h | all`; `boot` and a relative window
/// are alternative answers to the same question and are never combined.
///
/// `since_ms` resolves to an ABSOLUTE epoch freshly from `now` at request
/// time, not anchored to when the view opened -- each read is
/// self-consistent, but the window creeps forward across a long-lived view.
/// `None` on invalid input, which the caller turns into a 400.
fn resolve_log_filters(priority: Option<u32>, window: Option<&str>) -> Option<LogFilters> {
    let max_priority = match priority {
        None => 0,
        Some(p) if p <= 7 => p,
        Some(_) => return None,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    let (since_ms, current_boot) = match window.unwrap_or("all") {
        "all" => (0, false),
        "boot" => (0, true),
        "1h" => (now_ms.saturating_sub(3_600_000), false),
        "24h" => (now_ms.saturating_sub(86_400_000), false),
        _ => return None,
    };
    Some(LogFilters {
        max_priority,
        since_ms,
        current_boot,
    })
}

/// `resolve_log_filters` plus an explicit, already-resolved cutoff: a page
/// read must use the SAME cutoff as its stream, or a long-lived view finds
/// each page truncated to a fresh `now` instead of its original window.
/// `since_ms` wins over `window` (still validated even when overridden).
///
/// `Some(0)` = "no explicit cutoff" (the `since_ms==0` means unset
/// convention used everywhere in this codebase, see
/// `crates/agent/src/logs.rs`) -- NOT hypothetical: `window=boot` resolves
/// to `since_ms=0`, so the `meta` frame announces it and the client echoes
/// it back on every page read. Treating that as authoritative would
/// silently drop the boot filter on every subsequent page.
fn resolve_log_filters_with_since(
    priority: Option<u32>,
    window: Option<&str>,
    since_ms: Option<u64>,
) -> Option<LogFilters> {
    let mut f = resolve_log_filters(priority, window)?;
    if let Some(explicit) = since_ms.filter(|&v| v != 0) {
        f.since_ms = explicit;
        f.current_boot = false;
    }
    Some(f)
}

/// Journal-only concept -- `run_docker` ignores filters entirely. Zeroed for
/// any non-journal source before dispatch/audit, so what's audited matches
/// what the agent actually did, not a filter that was silently ignored.
fn filters_for_source(source: &str, filters: LogFilters) -> LogFilters {
    if source.starts_with("journal:") {
        filters
    } else {
        LogFilters::default()
    }
}

/// The audit `target` for a log read: the source plus whatever narrowed it. A
/// filtered read and a full read are different disclosures, so the trail records
/// what was actually read.
fn audit_target(source: &str, f: &LogFilters) -> String {
    let mut s = source.to_string();
    if f.max_priority > 0 {
        s.push_str(&format!(" p<={}", f.max_priority));
    }
    if f.current_boot {
        s.push_str(" boot");
    } else if f.since_ms > 0 {
        s.push_str(&format!(" since={}", f.since_ms));
    }
    s
}

/// Sends `LogTailStop` when the SSE response is dropped (tab closed, nav
/// away, connection lost) -- the only thing stopping a `journalctl -f` from
/// outliving the view, so this must stay owned by the stream.
struct TailGuard {
    hub: Arc<Hub>,
    machine_id: Uuid,
    request_id: String,
}

impl Drop for TailGuard {
    fn drop(&mut self) {
        self.hub.close_tail(&self.request_id);
        let hub = self.hub.clone();
        let machine_id = self.machine_id;
        let request_id = self.request_id.clone();
        // Drop is sync; the stop is a send, so it needs a task.
        tokio::spawn(async move {
            if let Err(e) = hub.send_log_stop(machine_id, request_id).await {
                tracing::debug!(error = ?e, "log tail: stop not delivered (agent gone)");
            }
        });
    }
}

/// `GET /api/machines/{id}/logs/stream?source=&tail=&follow=` — open a tail on
/// the agent and stream it to the browser as SSE.
async fn log_stream(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<LogStreamQuery>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    if !source_is_valid(&q.source) {
        return (StatusCode::BAD_REQUEST, "invalid source").into_response();
    }
    let Some(filters) = resolve_log_filters(q.priority, q.window.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "invalid priority or window").into_response();
    };
    let filters = filters_for_source(&q.source, filters);
    let tail = q.tail.unwrap_or(DEFAULT_TAIL_LINES).min(MAX_TAIL_LINES);
    let follow = q.follow.unwrap_or(true);

    // Open the tail and dispatch LogTailStart BEFORE auditing: an offline
    // agent must 409 with NO `logs.open` row at all, rather than leaving a
    // misleading `logs.open`/`ok` row behind for a read that never happened.
    let (request_id, rx) = state.hub.open_tail(id);
    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_log_start(
            id,
            request_id.clone(),
            q.source.clone(),
            tail,
            follow,
            String::new(),
            filters,
        )
        .await
    {
        state.hub.close_tail(&request_id);
        return (StatusCode::CONFLICT, "agent not connected").into_response();
    }

    // Reading logs isn't a mutation but can expose secrets, so who read what
    // is recorded (PRD treats `terminal.open` the same way) -- written once
    // as `ok`, only now that the tail is actually live on the agent.
    let command_id = Uuid::new_v4();
    if let Err(e) = repo::audit_command(
        &state.pool,
        repo::Actor::User(&identity),
        "logs.open",
        Some(id),
        &audit_target(&q.source, &filters),
        command_id,
        "ok",
    )
    .await
    {
        // The tail is already live on the agent; since we can't audit it,
        // fail closed by tearing it back down rather than leaving an
        // unaudited stream running.
        tracing::error!(error = %e, "log stream: audit write failed; closing tail");
        state.hub.close_tail(&request_id);
        if let Err(e) = state.hub.send_log_stop(id, request_id).await {
            tracing::debug!(error = ?e, "log stream: stop not delivered (agent gone)");
        }
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to record audit entry",
        )
            .into_response();
    }

    let guard = TailGuard {
        hub: state.hub.clone(),
        machine_id: id,
        request_id,
    };

    // Announced FIRST so every page read of this tail can echo it back
    // instead of re-resolving `now` and drifting. A *named* event
    // deliberately: EventSource routes named events to addEventListener,
    // not onmessage, so this can't be mistaken for a log line.
    let meta = Event::default()
        .event("meta")
        .data(format!(r#"{{"since_ms":{}}}"#, filters.since_ms));
    let head = tokio_stream::once(Ok::<Event, Infallible>(meta));

    let stream = ReceiverStream::new(rx).map(move |chunk| {
        // The guard is owned by the closure, so it drops with the stream.
        let _ = &guard;
        Ok::<Event, Infallible>(Event::default().data(String::from_utf8_lossy(&chunk.data)))
    });

    Sse::new(head.chain(stream))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// A page read is a one-shot; bound the collection so a wedged agent can't hang
/// the request. journalctl returns a bounded page quickly.
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// One backward page, collected from a short-lived non-follow tail. Journal
/// only; docker has no cursor.
async fn logs_page(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<LogPageQuery>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    if !source_is_valid(&q.source) || !q.source.starts_with("journal:") {
        return (StatusCode::BAD_REQUEST, "invalid or non-journal source").into_response();
    }
    let Some(before) = q.before.filter(|b| !b.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "missing `before` cursor").into_response();
    };
    let Some(filters) = resolve_log_filters_with_since(q.priority, q.window.as_deref(), q.since_ms)
    else {
        return (
            StatusCode::BAD_REQUEST,
            "invalid priority, window or since_ms",
        )
            .into_response();
    };
    // Always a no-op today (the check above already rejects non-journal
    // sources), kept for defense-in-depth so this handler can never dispatch
    // or audit a filter a source doesn't support.
    let filters = filters_for_source(&q.source, filters);
    let limit = q.limit.unwrap_or(DEFAULT_TAIL_LINES).min(MAX_TAIL_LINES);

    // Dispatch BEFORE auditing (mirrors `log_stream`): an offline agent must
    // 409 with NO `logs.page` row, and a timeout must not leave an `ok` row
    // for a read that never returned.
    let (request_id, mut rx) = state.hub.open_tail(id);
    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_log_start(
            id,
            request_id.clone(),
            q.source.clone(),
            limit,
            false,
            before,
            filters,
        )
        .await
    {
        state.hub.close_tail(&request_id);
        return (StatusCode::CONFLICT, "agent not connected").into_response();
    }

    // Same posture as `logs.open` (see its comment): recorded only once the
    // read is actually live, fail closed if the row can't be written.
    let command_id = Uuid::new_v4();
    if let Err(e) = repo::audit_command(
        &state.pool,
        repo::Actor::User(&identity),
        "logs.page",
        Some(id),
        &audit_target(&q.source, &filters),
        command_id,
        "ok",
    )
    .await
    {
        tracing::error!(error = %e, "logs page: audit write failed; closing tail");
        state.hub.close_tail(&request_id);
        if let Err(e) = state.hub.send_log_stop(id, request_id).await {
            tracing::debug!(error = ?e, "logs page: stop not delivered (agent gone)");
        }
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to record audit entry",
        )
            .into_response();
    }

    // Agent sends the whole page, then an eof chunk.
    let mut buf: Vec<u8> = Vec::new();
    let collected = tokio::time::timeout(PAGE_TIMEOUT, async {
        while let Some(chunk) = rx.recv().await {
            buf.extend_from_slice(&chunk.data);
            if chunk.eof {
                break;
            }
        }
    })
    .await;
    // Tear down the agent-side tail too: the read process exits on its own,
    // but the agent only drops its `AbortHandle` entry on `LogTailStop` --
    // without this, each page fetch leaks a dead handle for the session's life.
    state.hub.close_tail(&request_id);
    if let Err(e) = state.hub.send_log_stop(id, request_id).await {
        tracing::debug!(error = ?e, "logs page: stop not delivered (agent gone)");
    }
    if collected.is_err() {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            "agent did not return a page in time",
        )
            .into_response();
    }

    // Parse the NDJSON page. Lines are already oldest-first from the agent.
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&buf)
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    let oldest_cursor = lines.iter().find_map(|l| {
        l.get("cursor")
            .and_then(|c| c.as_str())
            .map(|c| c.to_string())
    });
    let reached_start = (lines.len() as u32) < limit;

    Json(LogPage {
        lines,
        oldest_cursor,
        reached_start,
    })
    .into_response()
}

/// The bounded wait for a dispatched verb's result.
const VERB_TIMEOUT: Duration = Duration::from_secs(10);

/// JSON returned by a verb POST — `ok`/`message` are present on completion,
/// absent when we returned before the agent replied (202 pending).
#[derive(serde::Serialize)]
struct VerbResult {
    command_id: String,
    ok: Option<bool>,
    message: Option<String>,
    status: &'static str,
}

async fn container_action(
    State(state): State<AppState>,
    Path((id, container, action)): Path<(Uuid, String, String)>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    let verb = match action.as_str() {
        "start" => Verb::ContainerStart,
        "stop" => Verb::ContainerStop,
        "restart" => Verb::ContainerRestart,
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    run_verb(
        &state,
        id,
        verb,
        &container,
        &format!("container.{action}"),
        VERB_TIMEOUT,
        &identity,
    )
    .await
}

async fn unit_action(
    State(state): State<AppState>,
    Path((id, unit, action)): Path<(Uuid, String, String)>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    let verb = match action.as_str() {
        "start" => Verb::UnitStart,
        "stop" => Verb::UnitStop,
        "restart" => Verb::UnitRestart,
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    // A unit name is never empty and never contains '/'; reject rather than
    // forward something the agent would only fail on. (Empty is unreachable
    // through the router -- matchit won't bind it -- kept for any direct caller.)
    if unit.is_empty() || unit.contains('/') {
        return (StatusCode::BAD_REQUEST, "invalid unit name").into_response();
    }
    run_verb(
        &state,
        id,
        verb,
        &unit,
        &format!("unit.{action}"),
        VERB_TIMEOUT,
        &identity,
    )
    .await
}

/// The shared verb pipeline: audit-before-dispatch (fail closed), dispatch,
/// then a bounded wait for the result. `timeout` is injected so tests don't
/// wait the full 10s. `identity` attributes both the audit row and the
/// `issued_by` the agent sees on the wire to the real operator.
async fn run_verb(
    state: &AppState,
    id: Uuid,
    verb: Verb,
    target: &str,
    audit_action: &str,
    timeout: Duration,
    identity: &repo::Identity,
) -> Response {
    let command_id = Uuid::new_v4();
    let cid = command_id.to_string();

    // Registered AND audited BEFORE dispatch: the row must exist before the
    // agent can round-trip a CommandResult, whose UPDATE is keyed by
    // command_id and would otherwise silently no-op against a not-yet-inserted
    // row, freezing it at "dispatched" forever.
    //
    // NOTE: "dispatched" is NOT guaranteed-terminal -- do not assume a row's
    // result is final. A session ending before a spawned verb task finishes
    // (up to 90s) drops the CommandResult; reconciling stale rows is
    // deliberately deferred to a scheduled job (CLAUDE.md's apalis-vs-pgmq gate).
    let rx = state.hub.register_pending(cid.clone(), id);
    if let Err(e) = repo::audit_command(
        &state.pool,
        repo::Actor::User(identity),
        audit_action,
        Some(id),
        target,
        command_id,
        "dispatched",
    )
    .await
    {
        // Fail closed: a verb must never execute unaudited (CLAUDE.md). If the
        // dispatched audit write fails, abandon the waiter and do NOT dispatch.
        state.hub.abandon_pending(&cid);
        tracing::error!(error = %e, action = audit_action, "verb: dispatched audit write failed; not dispatching");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to record audit entry",
        )
            .into_response();
    }

    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_command(
            id,
            cid.clone(),
            verb,
            target.to_string(),
            repo::Actor::User(identity),
        )
        .await
    {
        state.hub.abandon_pending(&cid);
        // Agent offline: no CommandResult will ever arrive, so flip to
        // terminal "denied" here -- the one case the grpc CommandResult arm
        // can't cover, so it doesn't conflict with that arm being the sole
        // writer of a real ok/error result.
        if let Err(e) = repo::update_command_result(&state.pool, command_id, id, "denied").await {
            tracing::error!(error = %e, "verb: denied audit update failed");
        }
        return (StatusCode::CONFLICT, "agent not connected").into_response();
    }

    match tokio::time::timeout(timeout, rx).await {
        // The gRPC CommandResult arm already updated the audit row's result.
        Ok(Ok(result)) => Json(VerbResult {
            command_id: cid,
            ok: Some(result.ok),
            message: Some(result.message),
            status: "completed",
        })
        .into_response(),
        Ok(Err(_)) => (StatusCode::INTERNAL_SERVER_ERROR, "result channel closed").into_response(),
        Err(_) => {
            state.hub.abandon_pending(&cid);
            (
                StatusCode::ACCEPTED,
                Json(VerbResult {
                    command_id: cid,
                    ok: None,
                    message: None,
                    status: "pending",
                }),
            )
                .into_response()
        }
    }
}

/// Serve the embedded React app, falling back to `index.html` for client-side
/// routes (PRD §10).
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(content) => asset_response(path, content),
        None => match Assets::get("index.html") {
            Some(index) => asset_response("index.html", index),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

/// Vite writes every hashed artifact under `assets/` (a content change means a
/// new filename), so those are safe to cache forever; everything else --
/// index.html above all -- must revalidate, or a deploy would strand browsers
/// on an index pointing at asset hashes that no longer exist.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// In release builds `EmbeddedFile.data` is `Cow::Borrowed(&'static [u8])`;
/// `into_owned()` would memcpy the whole asset (the main JS chunk is ~1.5 MB)
/// on every request. `Bytes::from_static` serves the embedded bytes directly.
fn asset_response(path: &str, content: rust_embed::EmbeddedFile) -> Response {
    let body = match content.data {
        std::borrow::Cow::Borrowed(b) => axum::body::Bytes::from_static(b),
        std::borrow::Cow::Owned(v) => axum::body::Bytes::from(v),
    };
    (
        [
            (header::CONTENT_TYPE, content_type(path)),
            (header::CACHE_CONTROL, cache_control(path)),
        ],
        body,
    )
        .into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::Hub;
    use argus_proto::v1::{server_frame, CommandResult, ServerFrame};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use base64::Engine;
    use tokio::sync::mpsc;
    use tonic::Status;
    use tower::ServiceExt;

    #[sqlx::test]
    async fn audit_endpoint_rejects_bad_params(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        for uri in [
            "/api/audit?category=nonsense",
            "/api/audit?result=maybe",
            "/api/audit?window=1y",
            "/api/audit?limit=0",
            "/api/audit?limit=501",
            "/api/audit?machine=not-a-uuid",
            "/api/audit?before_id=0",
            // Unknown param NAME (typo), not just an unknown value -- must
            // 400 via `deny_unknown_fields`, not fall through unfiltered.
            "/api/audit?categry=auth",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::get(uri)
                        .header("cookie", &cookie)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(
                res.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for {uri}"
            );
        }
        Ok(())
    }

    #[sqlx::test]
    async fn audit_endpoint_returns_filtered_rows(pool: PgPool) -> anyhow::Result<()> {
        sqlx::query!(
            "INSERT INTO audit_log (actor, action, result) VALUES ('system', 'auth.login', 'ok')"
        )
        .execute(&pool)
        .await?;
        sqlx::query!(
            "INSERT INTO audit_log (actor, action, result) VALUES ('system', 'unit.stop', 'denied')"
        )
        .execute(&pool)
        .await?;

        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        let res = app
            .clone()
            .oneshot(
                Request::get("/api/audit?category=auth&window=all")
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await?;
        let page: serde_json::Value = serde_json::from_slice(&body)?;
        let rows = page["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["action"], "auth.login");
        assert_eq!(rows[0]["hostname"], serde_json::Value::Null);
        // Absent-key guard, same reasoning as the fleet test above.
        assert!(rows[0].as_object().unwrap().contains_key("detail"));
        assert_eq!(page["has_more"], false);

        // Unauthenticated -> 401 from the shared middleware.
        let res = app
            .oneshot(Request::get("/api/audit").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[sqlx::test]
    async fn readyz_reports_ready_with_a_live_database(pool: PgPool) -> anyhow::Result<()> {
        let app = router(test_state(pool));
        let res = app
            .oneshot(Request::get("/readyz").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        Ok(())
    }

    #[sqlx::test]
    async fn fleet_lists_machines_with_status(pool: PgPool) -> anyhow::Result<()> {
        // Hostnames are chosen so `ORDER BY hostname` deterministically puts
        // the online machine first and the offline one second.
        sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, os, primary_ip, status, last_seen_at)
               VALUES ('test-fleet-online', 'a-online-host', 'Debian 12', '10.0.0.5'::inet, 'online', now())"#
        )
        .execute(&pool)
        .await?;

        sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status)
               VALUES ('test-fleet-offline', 'z-offline-host', 'offline')"#
        )
        .execute(&pool)
        .await?;

        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/fleet")
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["hostname"], "a-online-host");
        assert_eq!(rows[0]["status"], "online");
        // `contains_key` first: indexing a Value returns Null for an ABSENT
        // key too, so the `assert_eq!` alone would keep passing if the field
        // were dropped from FleetRow entirely.
        let row_obj = rows[0].as_object().unwrap();
        assert!(
            row_obj.contains_key("display_name"),
            "display_name must be in the fleet payload"
        );
        assert_eq!(row_obj["display_name"], serde_json::Value::Null);
        assert!(row_obj.contains_key("capabilities"));
        assert_eq!(rows[1]["hostname"], "z-offline-host");
        assert_eq!(rows[1]["status"], "offline");

        Ok(())
    }

    #[sqlx::test]
    async fn fleet_machine_and_metrics_endpoints(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, os, status)
               VALUES ('test-metrics-api-machine', 'metrics-api-host', 'Debian 12', 'online')
               RETURNING id"#
        )
        .fetch_one(&pool)
        .await?
        .id;

        // Explicit, strictly-increasing `ts` values (rather than three back-to-
        // back `now()`s) so the metrics-history ordering assertion below can't
        // flake on same-microsecond timestamps.
        for (i, cpu) in [10.0_f32, 20.0, 30.0].into_iter().enumerate() {
            sqlx::query!(
                r#"
                INSERT INTO metrics (machine_id, ts, cpu_pct, mem_used, mem_total, swap_used,
                    swap_total, load1, load5, load15, disk_used, disk_total, net_rx_bytes,
                    net_tx_bytes, uptime_secs)
                VALUES ($1, now() - (interval '1 minute' * $2::float8), $3, 1000, 4000, 0, 0,
                        0.1, 0.1, 0.1, 1, 1, 1, 1, 1)
                "#,
                machine_id,
                (2 - i) as f64,
                cpu,
            )
            .execute(&pool)
            .await?;
        }

        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        // /api/fleet: the seeded machine's row must carry a non-empty
        // spark_cpu ending in the latest sample.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/fleet")
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows.len(), 1);
        let spark_cpu = rows[0]["spark_cpu"].as_array().expect("spark_cpu array");
        assert_eq!(spark_cpu.len(), 3);
        assert_eq!(rows[0]["cpu_pct"], 30.0);

        // /api/machines/{id}: 200 with the right hostname.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{machine_id}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let detail: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(detail["hostname"], "metrics-api-host");

        // /api/machines/{random}: 404.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{}", Uuid::new_v4()))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // /api/machines/{id}/metrics?range=1h: ascending array of all 3 samples.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{machine_id}/metrics?range=1h"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let points: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(points.len(), 3);
        let timestamps: Vec<&str> = points.iter().map(|p| p["ts"].as_str().unwrap()).collect();
        let mut sorted = timestamps.clone();
        sorted.sort();
        assert_eq!(timestamps, sorted, "expected ascending order by ts");
        assert_eq!(points[0]["cpu_pct"], 10.0);
        assert_eq!(points[2]["cpu_pct"], 30.0);

        // /api/machines/{id}/metrics?range=bogus: 400.
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{machine_id}/metrics?range=bogus"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    /// Pins the capability tri-state through the actual `GET
    /// /api/machines/{id}` JSON body -- the contract
    /// `MachineDetailPage.tsx`'s `caps`/`lacks()` reads. `null` must gate
    /// NOTHING (predates capability reporting); `[]` must gate EVERYTHING.
    #[sqlx::test]
    async fn machine_detail_json_carries_the_capability_tri_state(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let null_id: Uuid = sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status)
               VALUES ('caps-null', 'caps-null-host', 'online') RETURNING id"#
        )
        .fetch_one(&pool)
        .await?
        .id;

        let empty_id: Uuid = sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status, capabilities)
               VALUES ('caps-empty', 'caps-empty-host', 'online', ARRAY[]::text[])
               RETURNING id"#
        )
        .fetch_one(&pool)
        .await?
        .id;

        let populated_id: Uuid = sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status, capabilities)
               VALUES ('caps-populated', 'caps-populated-host', 'online', ARRAY['systemd', 'journal'])
               RETURNING id"#
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        for (id, expected) in [
            (null_id, serde_json::Value::Null),
            (empty_id, serde_json::json!([])),
            (populated_id, serde_json::json!(["systemd", "journal"])),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/machines/{id}"))
                        .header("cookie", &cookie)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX).await?;
            let detail: serde_json::Value = serde_json::from_slice(&body)?;
            assert_eq!(
                detail["capabilities"], expected,
                "capabilities JSON mismatch for machine {id}"
            );
        }

        Ok(())
    }

    /// The four inventory columns ride along like `capabilities`: present
    /// with values when reported, present as explicit `null` when not.
    /// Checked with `contains_key`, never bare indexing (same `Value::Null`
    /// for-absent-key gotcha as the fleet test above).
    #[sqlx::test]
    async fn machine_detail_json_carries_inventory_fields(pool: PgPool) -> anyhow::Result<()> {
        let with_id: Uuid = sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status, cpu_model, cpu_cores, boot_time, virt)
               VALUES ('inv-full', 'inv-full-host', 'online', 'AMD Ryzen 7 5800X', 8, to_timestamp(1700000000), 'kvm')
               RETURNING id"#
        )
        .fetch_one(&pool)
        .await?
        .id;

        let without_id: Uuid = sqlx::query!(
            r#"INSERT INTO machines (machine_id, hostname, status)
               VALUES ('inv-none', 'inv-none-host', 'online') RETURNING id"#
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        for id in [with_id, without_id] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/machines/{id}"))
                        .header("cookie", &cookie)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX).await?;
            let detail: serde_json::Value = serde_json::from_slice(&body)?;
            let obj = detail.as_object().expect("detail body is a JSON object");

            for key in ["cpu_model", "cpu_cores", "boot_time", "virt"] {
                assert!(
                    obj.contains_key(key),
                    "key {key} must be present in the detail payload for machine {id}"
                );
            }

            if id == with_id {
                assert_eq!(detail["cpu_model"], serde_json::json!("AMD Ryzen 7 5800X"));
                assert_eq!(detail["cpu_cores"], serde_json::json!(8));
                assert_eq!(detail["virt"], serde_json::json!("kvm"));
                assert!(
                    detail["boot_time"].is_string(),
                    "boot_time must serialize as an RFC3339 string when reported"
                );
            } else {
                assert_eq!(detail["cpu_model"], serde_json::Value::Null);
                assert_eq!(detail["cpu_cores"], serde_json::Value::Null);
                assert_eq!(detail["boot_time"], serde_json::Value::Null);
                assert_eq!(detail["virt"], serde_json::Value::Null);
            }
        }

        Ok(())
    }

    fn app_state_with_hub(pool: PgPool) -> (AppState, Arc<Hub>) {
        let hub = Arc::new(Hub::new());
        let mut state = test_state(pool);
        state.hub = hub.clone();
        (state, hub)
    }

    /// A stand-in `Identity` for tests that call handlers directly rather
    /// than through the router (bypassing `require_auth`).
    fn test_identity() -> repo::Identity {
        repo::Identity {
            subject: "test-subject".into(),
            email: Some("test@example.com".into()),
            display_name: None,
        }
    }

    /// Seeds a live session and returns a `Cookie` header value: every
    /// `/api` route now sits behind `require_auth`, so a request with no
    /// cookie 401s before reaching the handler under test.
    async fn auth_cookie(pool: &PgPool) -> anyhow::Result<String> {
        let (token, hash) = crate::auth::session::new_session_token();
        repo::create_session(
            pool,
            &hash,
            &test_identity(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await?;
        Ok(format!("{}={}", argus_common::SESSION_COOKIE, token))
    }

    /// Consolidated so body-carrying tests (`PATCH`/`POST` handlers) don't
    /// repeat the same five lines per call.
    async fn request_json(
        app: &Router,
        method: &str,
        uri: &str,
        cookie: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// The bodyless (`GET`/`DELETE`) counterpart to `request_json`.
    async fn request(
        app: &Router,
        method: &str,
        uri: &str,
        cookie: &str,
    ) -> axum::http::Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Pairs with `request`/`request_json`, skipping the `to_bytes` dance.
    async fn body_json<T: serde::de::DeserializeOwned>(
        res: axum::http::Response<Body>,
    ) -> anyhow::Result<T> {
        let body = to_bytes(res.into_body(), usize::MAX).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    #[sqlx::test]
    async fn token_mint_defaults_and_raw_once(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool.clone()));
        let res = request_json(
            &app,
            "POST",
            "/api/enrollment-tokens",
            &cookie,
            serde_json::json!({ "name": "pve1-media", "tags": ["media"] }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: serde_json::Value = body_json(res).await?;
        let raw = body["token"].as_str().unwrap();
        assert_eq!(raw.len(), 32);
        assert_eq!(body["max_uses"], 1);
        // Not just non-null: pinned to ~now+24h so a default regression to,
        // say, +1h fails here instead of only in the wild.
        let expires_at_str = body["expires_at"].as_str().expect("expires_at string");
        let expires_at = OffsetDateTime::parse(
            expires_at_str,
            &time::format_description::well_known::Rfc3339,
        )
        .expect("expires_at must be RFC3339");
        let now = OffsetDateTime::now_utc();
        assert!(
            expires_at > now + time::Duration::hours(23)
                && expires_at < now + time::Duration::hours(25),
            "expected expires_at ~24h from now, got {expires_at} (now = {now})"
        );

        // The raw token is NOT in the list payload -- only in the mint response.
        let list = request(&app, "GET", "/api/enrollment-tokens", &cookie).await;
        let rows: serde_json::Value = body_json(list).await?;
        assert!(rows[0].get("token").is_none());
        assert!(rows[0].get("token_hash").is_none());

        // The minted token actually works against the consume path.
        let check = repo::consume_enrollment_token(&pool, raw).await?;
        assert!(matches!(check, repo::TokenCheck::Valid { .. }));
        // ...and, being single-use, only once.
        let again = repo::consume_enrollment_token(&pool, raw).await?;
        assert!(matches!(again, repo::TokenCheck::Invalid));
        Ok(())
    }

    #[sqlx::test]
    async fn token_revoke_and_audit_rows(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool.clone()));
        let res = request_json(
            &app,
            "POST",
            "/api/enrollment-tokens",
            &cookie,
            serde_json::json!({ "name": "t", "max_uses": null }),
        )
        .await;
        let body: serde_json::Value = body_json(res).await?;
        assert!(body["max_uses"].is_null()); // explicit null = unlimited
        let id = body["id"].as_str().unwrap().to_string();
        let raw = body["token"].as_str().unwrap().to_string();

        let del = request(
            &app,
            "DELETE",
            &format!("/api/enrollment-tokens/{id}"),
            &cookie,
        )
        .await;
        assert_eq!(del.status(), StatusCode::NO_CONTENT);
        // Revoked -> the consume path refuses it.
        let check = repo::consume_enrollment_token(&pool, &raw).await?;
        assert!(matches!(check, repo::TokenCheck::Invalid));

        // This test mints exactly one token (one `enroll_token.create`) and
        // revokes it (one `enroll_token.revoke`).
        let n: i64 = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!" FROM audit_log
               WHERE action IN ('enroll_token.create', 'enroll_token.revoke')"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(n, 2);
        Ok(())
    }

    #[sqlx::test]
    async fn token_revoke_unknown_id_is_404(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));
        let res = request(
            &app,
            "DELETE",
            &format!("/api/enrollment-tokens/{}", Uuid::new_v4()),
            &cookie,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[sqlx::test]
    async fn token_mint_rejects_empty_name_and_clamps_bounds(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        // Whitespace-only name normalizes to empty and is rejected.
        let res = request_json(
            &app,
            "POST",
            "/api/enrollment-tokens",
            &cookie,
            serde_json::json!({ "name": "   " }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // max_uses below 1 clamps up to 1; expires_in_hours above the cap
        // clamps down to 8760 (1 year), rather than being rejected.
        let res = request_json(
            &app,
            "POST",
            "/api/enrollment-tokens",
            &cookie,
            serde_json::json!({ "name": "clamp-test", "max_uses": 0, "expires_in_hours": 999_999 }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: serde_json::Value = body_json(res).await?;
        assert_eq!(body["max_uses"], 1);
        Ok(())
    }

    /// `make_interval` propagates a NULL `hours` straight through to a NULL
    /// result -- what lets `mint_enrollment_token` store "never expires" by
    /// passing `None` with no `CASE`. Pinned so a future Postgres version
    /// changing this fails loudly in CI, not by silently expiring
    /// "unlimited" tokens.
    #[sqlx::test]
    async fn make_interval_of_null_hours_is_null(pool: PgPool) -> anyhow::Result<()> {
        let expires_at: Option<OffsetDateTime> = sqlx::query_scalar!(
            r#"SELECT now() + make_interval(hours => $1) as "expires_at""#,
            None::<i32>,
        )
        .fetch_one(&pool)
        .await?;
        assert!(expires_at.is_none());
        Ok(())
    }

    #[sqlx::test]
    async fn ca_pem_503_when_ca_not_initialized(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));
        let res = request(&app, "GET", "/api/ca.pem", &cookie).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        Ok(())
    }

    /// The scriptable-bootstrap route (design: the CA is public by
    /// definition; a host being enrolled has no session to present). The
    /// authenticated twin must stay authenticated -- the /api blanket rule
    /// is intact, this route simply lives outside it.
    #[sqlx::test]
    async fn public_ca_pem_serves_without_a_session(pool: PgPool) -> anyhow::Result<()> {
        sqlx::query!(
            "INSERT INTO ca_material (id, cert_pem, key_ciphertext, key_nonce)
             VALUES (1, '-----BEGIN CERTIFICATE-----test', '\\x00'::bytea, '\\x00'::bytea)"
        )
        .execute(&pool)
        .await?;
        let app = router(test_state(pool));

        // NO cookie on purpose -- this is the whole point of the route.
        let res = app
            .clone()
            .oneshot(Request::get("/ca.pem").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await?;
        assert!(body.starts_with(b"-----BEGIN CERTIFICATE-----"));

        // The /api twin still 401s without a session: public /ca.pem must
        // not have been achieved by weakening the /api layer.
        let res = app
            .oneshot(Request::get("/api/ca.pem").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    /// The Enroll page's endpoint interpolation source: server-composed
    /// URLs (SAN + agent port), authenticated like everything under /api.
    #[sqlx::test]
    async fn enrollment_config_returns_composed_endpoints(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool));

        let res = app
            .clone()
            .oneshot(Request::get("/api/enrollment-config").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "must require auth");

        let res = request(&app, "GET", "/api/enrollment-config", &cookie).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            v["agent_endpoints"],
            serde_json::json!(["https://agents.test:9443", "https://10.0.0.5:9443"]),
            "endpoints come through in ARGUS_AGENT_SANS order"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn get_docker_returns_cached_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool);
        let id = Uuid::new_v4();

        // empty before any report
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/docker"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert!(rows.is_empty());

        // populate the cache, then it shows up
        hub.set_docker(
            id,
            vec![argus_proto::v1::Container {
                id: "deadbeef".into(),
                name: "grafana".into(),
                image: "grafana/grafana".into(),
                state: "running".into(),
                status: "Up 1 hour".into(),
                health: String::new(),
            }],
        );
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/docker"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "grafana");

        Ok(())
    }

    #[sqlx::test]
    async fn verb_on_offline_agent_returns_409_and_audits_denied(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('verb-offline', 'h', 'offline') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = run_verb(
            &state,
            machine_id,
            Verb::ContainerRestart,
            "web",
            "container.restart",
            Duration::from_millis(200),
            &test_identity(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let row = sqlx::query!(
            "SELECT result FROM audit_log WHERE machine_id = $1 AND action = 'container.restart'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("denied"));

        Ok(())
    }

    #[sqlx::test]
    async fn verb_with_connected_agent_completes_ok(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('verb-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: register a connection and echo a success CommandResult for any
        // Command it receives (exactly what the real agent's session loop does).
        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = rx.recv().await {
                if let Some(server_frame::Payload::Command(cmd)) = frame.payload {
                    hub2.complete(
                        &cmd.command_id.clone(),
                        machine_id,
                        CommandResult {
                            command_id: cmd.command_id,
                            ok: true,
                            exit_code: 0,
                            message: "started".into(),
                        },
                    );
                }
            }
        });

        let resp = run_verb(
            &state,
            machine_id,
            Verb::ContainerStart,
            "web",
            "container.start",
            Duration::from_secs(5),
            &test_identity(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "completed");

        Ok(())
    }

    #[sqlx::test]
    async fn verb_times_out_to_202_when_agent_never_replies(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('verb-silent', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool.clone());
        // Register a connection whose receiver we hold but never reply on.
        let (tx, _rx_never) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);

        let resp = run_verb(
            &state,
            machine_id,
            Verb::ContainerStop,
            "web",
            "container.stop",
            Duration::from_millis(150),
            &test_identity(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(v["status"], "pending");

        Ok(())
    }

    #[sqlx::test]
    async fn verb_fails_closed_when_the_dispatched_audit_write_fails(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // A machine_id with NO `machines` row: `audit_command`'s FK to machines(id)
        // fails, exercising the dispatched-audit-write failure path.
        let ghost_id = Uuid::new_v4();
        let (state, hub) = app_state_with_hub(pool);

        // Register a live connection so send_command WOULD succeed if we reached it;
        // hold its receiver to prove no Command was dispatched.
        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(ghost_id, tx);

        let resp = run_verb(
            &state,
            ghost_id,
            Verb::ContainerStart,
            "web",
            "container.start",
            Duration::from_millis(200),
            &test_identity(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a failed dispatched-audit write must fail closed"
        );
        assert!(
            rx.try_recv().is_err(),
            "no Command frame may be dispatched when the audit write fails"
        );

        Ok(())
    }

    #[sqlx::test]
    async fn container_action_with_unknown_action_returns_400(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/machines/{}/docker/web/obliterate",
                        Uuid::new_v4()
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn get_systemd_returns_cached_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool);
        let id = Uuid::new_v4();

        // empty before any report
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/systemd"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert!(rows.is_empty());

        // populate the cache, then it shows up
        hub.set_systemd(
            id,
            vec![argus_proto::v1::Unit {
                name: "nginx.service".into(),
                load_state: "loaded".into(),
                active_state: "failed".into(),
                sub_state: "failed".into(),
                description: "A high performance web server".into(),
            }],
        );
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/systemd"))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "nginx.service");
        assert_eq!(rows[0]["active_state"], "failed");

        Ok(())
    }

    #[sqlx::test]
    async fn unit_verb_on_offline_agent_returns_409_and_audits_denied(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('unit-offline', 'h', 'offline') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = run_verb(
            &state,
            machine_id,
            Verb::UnitRestart,
            "nginx.service",
            "unit.restart",
            Duration::from_millis(200),
            &test_identity(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let row = sqlx::query!(
            "SELECT result, target_ref FROM audit_log WHERE machine_id = $1 AND action = 'unit.restart'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("denied"));
        assert_eq!(row.target_ref.as_deref(), Some("nginx.service"));

        Ok(())
    }

    #[sqlx::test]
    async fn unit_verb_with_connected_agent_completes_ok(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('unit-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: echo a success CommandResult, and record the verb it saw so
        // we can assert a UNIT verb (not a container one) went down the wire.
        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<i32>();
        tokio::spawn(async move {
            let mut seen_tx = Some(seen_tx);
            while let Some(Ok(frame)) = rx.recv().await {
                if let Some(server_frame::Payload::Command(cmd)) = frame.payload {
                    if let Some(s) = seen_tx.take() {
                        let _ = s.send(cmd.verb);
                    }
                    hub2.complete(
                        &cmd.command_id.clone(),
                        machine_id,
                        CommandResult {
                            command_id: cmd.command_id,
                            ok: true,
                            exit_code: 0,
                            message: "done".into(),
                        },
                    );
                }
            }
        });

        let resp = run_verb(
            &state,
            machine_id,
            Verb::UnitStart,
            "nginx.service",
            "unit.start",
            Duration::from_secs(5),
            &test_identity(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "completed");

        let seen = seen_rx.await?;
        assert_eq!(
            seen,
            Verb::UnitStart as i32,
            "a UNIT verb must ride the wire"
        );

        Ok(())
    }

    #[sqlx::test]
    async fn unit_action_rejects_a_malformed_unit_name(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);

        // `%2F` decodes to a slash inside the path segment -- must not be
        // forwarded to an agent (path-traversal-shaped input).
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/machines/{}/units/{}/start",
                        Uuid::new_v4(),
                        "..%2Fetc%2Fpasswd"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Assert on the body too: axum's own Path-extractor rejection is also a
        // 400, so the status alone wouldn't prove OUR validation is what fired.
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        assert_eq!(&body[..], b"invalid unit name");

        Ok(())
    }

    #[sqlx::test]
    async fn unit_action_with_unknown_action_returns_400(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/machines/{}/units/nginx.service/obliterate",
                        Uuid::new_v4()
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn fleet_reports_failed_unit_counts_from_the_hub(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('fleet-failed', 'fh', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool);

        // No snapshot yet -> 0, never null.
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/fleet")
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows[0]["failed_units"], 0);

        hub.set_systemd(
            machine_id,
            vec![
                argus_proto::v1::Unit {
                    name: "ok.service".into(),
                    load_state: "loaded".into(),
                    active_state: "active".into(),
                    sub_state: "running".into(),
                    description: String::new(),
                },
                argus_proto::v1::Unit {
                    name: "bad.service".into(),
                    load_state: "loaded".into(),
                    active_state: "failed".into(),
                    sub_state: "failed".into(),
                    description: String::new(),
                },
            ],
        );

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/fleet")
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows[0]["failed_units"], 1);

        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_rejects_a_bad_source(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        for bad in [
            "syslog:foo",
            "journal:",
            "journal:nginx%20service",
            "journal:..%2F..%2Fetc%2Fpasswd",
            "docker:abc%2Fdef",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/api/machines/{}/logs/stream?source={bad}",
                            Uuid::new_v4()
                        ))
                        .header("cookie", &cookie)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "source {bad} must be rejected"
            );
        }
        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_returns_409_when_the_agent_is_offline(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-offline', 'h', 'offline') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool.clone());
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=journal:nginx.service"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // Must be checked BEFORE the audit write (see `log_stream`'s comment):
        // a 409 must never leave a misleading `logs.open`/`ok` row behind.
        let row = sqlx::query!(
            "SELECT count(*) AS count FROM audit_log WHERE machine_id = $1 AND action = 'logs.open' AND result = 'ok'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.count,
            Some(0),
            "a 409 must not leave a logs.open/ok audit row"
        );

        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_opens_audits_and_streams(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: on LogTailStart, push one chunk then eof.
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    hub2.deliver_chunk(
                        &req.request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id.clone(),
                            data: b"{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"hello\"}\n"
                                .to_vec(),
                            eof: false,
                        },
                    );
                    let request_id = req.request_id.clone();
                    hub2.deliver_chunk(
                        &request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id,
                            data: Vec::new(),
                            eof: true,
                        },
                    );
                }
            }
        });

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=journal:nginx.service&tail=50"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("hello"),
            "SSE body must carry the chunk: {text}"
        );

        // The `meta` frame must be a NAMED event (see log_stream's comment on
        // why) and must arrive before the first log chunk.
        let meta_pos = text
            .find("event: meta\n")
            .expect("the SSE body must contain a frame named `meta`");
        let hello_pos = text.find("hello").expect("SSE body must carry the chunk");
        assert!(
            meta_pos < hello_pos,
            "the `meta` frame must be emitted before the first log chunk: {text}"
        );

        // The `meta` frame's `data:` field carries the resolved cutoff. No
        // window was requested, so it defaults to `all` (since_ms=0).
        let after_meta = &text[meta_pos..];
        let data_prefix = "data: ";
        let data_start = after_meta
            .find(data_prefix)
            .map(|i| i + data_prefix.len())
            .expect("the `meta` frame must carry a `data:` field");
        let data_end = after_meta[data_start..]
            .find('\n')
            .map(|i| data_start + i)
            .unwrap_or(after_meta.len());
        assert_eq!(
            &after_meta[data_start..data_end],
            r#"{"since_ms":0}"#,
            "the `meta` frame must announce the resolved cutoff: {text}"
        );

        let row = sqlx::query!(
            "SELECT action, target_ref, result FROM audit_log WHERE machine_id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.action, "logs.open");
        assert_eq!(row.target_ref.as_deref(), Some("journal:nginx.service"));
        assert_eq!(row.result.as_deref(), Some("ok"));

        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_clamps_an_oversized_tail(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-clamp', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool.clone());
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<u32>();
        tokio::spawn(async move {
            let mut seen_tx = Some(seen_tx);
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    if let Some(s) = seen_tx.take() {
                        let _ = s.send(req.tail_lines);
                    }
                }
            }
        });

        let app = router(state);
        let _ = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=journal:nginx.service&tail=999999"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(seen_rx.await?, MAX_TAIL_LINES, "tail must be clamped");
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_rejects_a_docker_source(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{}/logs/page?source=docker:abc&before=s%3Dx",
                        Uuid::new_v4()
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_rejects_a_non_numeric_since_ms(pool: PgPool) -> anyhow::Result<()> {
        // since_ms is Query<Option<u64>>; a non-numeric value fails axum's
        // deserialization before the handler body ever runs, so this
        // documents that the 400 comes from the extractor, not our code.
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{}/logs/page?source=journal:ssh.service&before=s%3Dx&since_ms=abc",
                        Uuid::new_v4()
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_requires_a_before_cursor(pool: PgPool) -> anyhow::Result<()> {
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{}/logs/page?source=journal:ssh.service",
                        Uuid::new_v4()
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_returns_409_when_offline(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-offline', 'h', 'offline') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool.clone());
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_collects_a_page_audits_and_reports_reached_start(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;
        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: on a LogTailStart with a before_cursor, stream two page
        // lines then eof.
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    assert_eq!(req.before_cursor, "s=x", "the cursor must reach the agent");
                    assert!(!req.follow, "a page read never follows");
                    let body = b"{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"older-a\",\"cursor\":\"s=a\"}\n{\"ts\":2,\"level\":6,\"ident\":null,\"msg\":\"older-b\",\"cursor\":\"s=b\"}\n".to_vec();
                    let request_id = req.request_id.clone();
                    hub2.deliver_chunk(
                        &request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id.clone(),
                            data: body,
                            eof: false,
                        },
                    );
                    hub2.deliver_chunk(
                        &request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id,
                            data: Vec::new(),
                            eof: true,
                        },
                    );
                }
            }
        });

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx&limit=500"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(v["lines"].as_array().unwrap().len(), 2);
        assert_eq!(v["lines"][0]["msg"], "older-a");
        assert_eq!(v["oldest_cursor"], "s=a");
        assert_eq!(
            v["reached_start"], true,
            "a short page means the journal start"
        );

        let row = sqlx::query!(
            "SELECT action, result FROM audit_log WHERE machine_id = $1 AND action = 'logs.page'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("ok"));

        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_rejects_an_out_of_range_priority(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-prio', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool).await?.id;
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = router(state)
            .oneshot(Request::builder()
                .uri(format!("/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx&priority=9"))
                .header("cookie", &cookie)
                .body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_rejects_an_out_of_range_priority(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('stream-prio', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool).await?.id;
        let cookie = auth_cookie(&pool).await?;
        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = router(state)
            .oneshot(Request::builder()
                .uri(format!("/api/machines/{machine_id}/logs/stream?source=journal:ssh.service&priority=8"))
                .header("cookie", &cookie)
                .body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_forwards_filters_and_audits_them(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-filt', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool).await?.id;
        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool.clone());
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    assert_eq!(req.max_priority, 4, "priority reaches the agent");
                    assert!(req.current_boot, "boot window reaches the agent");
                    assert_eq!(req.since_ms, 0, "boot and since are never both set");
                    let request_id = req.request_id.clone();
                    hub2.deliver_chunk(
                        &request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id,
                            data: Vec::new(),
                            eof: true,
                        },
                    );
                }
            }
        });
        let resp = router(state)
            .oneshot(Request::builder()
                .uri(format!("/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx&priority=4&window=boot"))
                .header("cookie", &cookie)
                .body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let row = sqlx::query!(
            "SELECT target_ref FROM audit_log WHERE machine_id = $1 AND action = 'logs.page'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        let target = row.target_ref.unwrap_or_default();
        assert!(
            target.contains("journal:ssh.service"),
            "source in the audit target"
        );
        assert!(target.contains("p<=4"), "priority recorded: {target}");
        assert!(target.contains("boot"), "window recorded: {target}");
        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_docker_source_ignores_filters_end_to_end(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-docker-filt', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let cookie = auth_cookie(&pool).await?;
        let (state, hub) = app_state_with_hub(pool.clone());
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    assert_eq!(
                        req.max_priority, 0,
                        "docker reads must not carry a priority filter"
                    );
                    assert!(
                        !req.current_boot,
                        "docker reads must not carry a boot filter"
                    );
                    assert_eq!(
                        req.since_ms, 0,
                        "docker reads must not carry a since filter"
                    );
                    let request_id = req.request_id.clone();
                    hub2.deliver_chunk(
                        &request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id,
                            data: Vec::new(),
                            eof: true,
                        },
                    );
                }
            }
        });

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=docker:abc&priority=4&window=boot"
                    ))
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let row = sqlx::query!(
            "SELECT target_ref FROM audit_log WHERE machine_id = $1 AND action = 'logs.open'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.target_ref.as_deref(),
            Some("docker:abc"),
            "a docker read's audit target must be bare, with no filter suffix the read never honoured"
        );

        Ok(())
    }

    // `resolve_log_filters`: the ONLY definition of the `window` -> proto
    // mapping. Direct, DB-free coverage so swapping the 3_600_000/86_400_000
    // constants can't slip past every other gate silently.

    #[test]
    fn resolve_log_filters_boot_sets_current_boot_and_no_since() {
        let f = resolve_log_filters(None, Some("boot")).expect("boot is a valid window");
        assert!(f.current_boot);
        assert_eq!(f.since_ms, 0);
    }

    #[test]
    fn resolve_log_filters_all_sets_neither_boot_nor_since() {
        let f = resolve_log_filters(None, Some("all")).expect("all is a valid window");
        assert!(!f.current_boot);
        assert_eq!(f.since_ms, 0);
    }

    #[test]
    fn resolve_log_filters_1h_cutoff_is_about_an_hour_before_now() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let f = resolve_log_filters(None, Some("1h")).expect("1h is a valid window");
        assert!(!f.current_boot);
        let expected = now_ms.saturating_sub(3_600_000);
        // Tolerance for the time elapsed between computing `now_ms` here and
        // the function computing its own `now` internally -- not an exact
        // equality against a freshly-taken timestamp.
        let delta = expected.abs_diff(f.since_ms);
        assert!(
            delta < 5_000,
            "since_ms should be ~now - 1h, delta={delta}ms"
        );
    }

    #[test]
    fn resolve_log_filters_24h_cutoff_is_strictly_older_than_1h_and_about_a_day_before_now() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let f24 = resolve_log_filters(None, Some("24h")).expect("24h is a valid window");
        let f1 = resolve_log_filters(None, Some("1h")).expect("1h is a valid window");
        assert!(!f24.current_boot);
        let expected = now_ms.saturating_sub(86_400_000);
        let delta = expected.abs_diff(f24.since_ms);
        assert!(
            delta < 5_000,
            "since_ms should be ~now - 24h, delta={delta}ms"
        );
        assert!(
            f24.since_ms < f1.since_ms,
            "the 24h cutoff must be strictly older than the 1h cutoff"
        );
    }

    #[test]
    fn resolve_log_filters_rejects_an_unknown_or_empty_window() {
        assert!(resolve_log_filters(None, Some("bogus")).is_none());
        assert!(resolve_log_filters(None, Some("")).is_none());
    }

    #[test]
    fn resolve_log_filters_priority_boundaries() {
        assert_eq!(
            resolve_log_filters(Some(0), Some("all"))
                .expect("0 is valid")
                .max_priority,
            0
        );
        assert_eq!(
            resolve_log_filters(Some(7), Some("all"))
                .expect("7 is valid")
                .max_priority,
            7
        );
        assert!(
            resolve_log_filters(Some(8), Some("all")).is_none(),
            "8 is out of the 0-7 syslog range"
        );
        assert_eq!(
            resolve_log_filters(None, Some("all"))
                .expect("absent priority is valid")
                .max_priority,
            0,
            "an absent priority defaults to 0 (unset), not rejected"
        );
    }

    #[test]
    fn explicit_since_ms_overrides_the_window() {
        // A page must be able to say "use the cutoff my stream was given".
        let f = resolve_log_filters_with_since(None, Some("1h"), Some(1_600_000_000_000))
            .expect("an explicit since_ms is valid");
        assert_eq!(f.since_ms, 1_600_000_000_000);
        assert!(!f.current_boot, "an explicit cutoff is not a boot window");
    }

    #[test]
    fn explicit_since_ms_turns_off_a_requested_boot_window() {
        // An explicit since_ms must win and flip current_boot back to false --
        // boot and an explicit cutoff must never combine. This is the
        // true->false transition; the `1h` test above never exercises it
        // (current_boot is already false there).
        let f = resolve_log_filters_with_since(None, Some("boot"), Some(1_600_000_000_000))
            .expect("a boot window plus an explicit since_ms is valid");
        assert_eq!(f.since_ms, 1_600_000_000_000);
        assert!(
            !f.current_boot,
            "an explicit since_ms must suppress the boot window"
        );
    }

    #[test]
    fn explicit_since_ms_of_zero_does_not_turn_off_a_requested_boot_window() {
        // `window=boot` resolves to `since_ms=0`, which must NOT be treated
        // as an explicit cutoff (see `resolve_log_filters_with_since`'s doc
        // for why `since_ms==0` means "unset").
        let f = resolve_log_filters_with_since(None, Some("boot"), Some(0))
            .expect("boot plus since_ms=0 is valid");
        assert!(
            f.current_boot,
            "since_ms=0 must not suppress a requested boot window"
        );
        assert_eq!(f.since_ms, 0);
    }

    #[test]
    fn without_since_ms_the_window_still_resolves() {
        let f = resolve_log_filters_with_since(None, Some("boot"), None)
            .expect("boot is a valid window");
        assert!(f.current_boot);
        assert_eq!(f.since_ms, 0);
    }

    #[test]
    fn an_invalid_window_is_still_rejected_even_with_since_ms() {
        assert!(resolve_log_filters_with_since(None, Some("bogus"), Some(1)).is_none());
    }

    // The PWA manifest must be served with the MIME type browsers require
    // to treat it as installable.

    #[test]
    fn content_type_serves_the_webmanifest_mime() {
        assert_eq!(
            content_type("manifest.webmanifest"),
            "application/manifest+json"
        );
    }

    /// Only Vite's content-hashed `assets/` may cache forever; index.html
    /// (and anything else) must revalidate, or a deploy strands browsers on
    /// an index pointing at asset hashes that no longer exist.
    #[test]
    fn cache_control_is_immutable_only_for_hashed_assets() {
        assert_eq!(
            cache_control("assets/index-DZ6.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control("assets/archivo-latin-400.woff2"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("favicon.svg"), "no-cache");
        // A client-side ROUTE served via the SPA fallback re-uses
        // index.html's policy, keyed by the resolved path, not the URI.
        assert_eq!(cache_control("machines/abc"), "no-cache");
    }

    fn test_state(pool: PgPool) -> AppState {
        let oidc = Arc::new(crate::config::OidcConfig {
            issuer: "https://idp.invalid".into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            required_role: crate::config::RequiredRole::Named("argus-admin".into()),
            roles_claim: "groups".into(),
            scopes: vec!["openid".into()],
            public_url: "http://localhost:8080".into(),
            ca_cert_path: None,
        });
        // Never triggers discovery (no test here drives /auth/login or
        // /auth/callback): building the client is local-only, so this is
        // cheap and does not touch the network.
        let oidc_client = Arc::new(
            crate::auth::oidc::OidcClient::new(oidc.clone()).expect("build test OIDC client"),
        );
        AppState {
            pool,
            hub: Arc::new(Hub::default()),
            oidc: Some(oidc),
            cipher: Arc::new(
                crate::crypto::FieldCipher::from_b64_key(
                    &base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
                )
                .expect("build test field cipher"),
            ),
            oidc_client: Some(oidc_client),
            public_url: "http://localhost:8080".into(),
            limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
            agent_endpoints: vec![
                "https://agents.test:9443".into(),
                "https://10.0.0.5:9443".into(),
            ],
        }
    }

    /// Every route class in one test, because the risk here is a route
    /// accidentally landing in the wrong group.
    #[sqlx::test]
    async fn public_routes_are_open_and_api_routes_are_not(pool: PgPool) -> anyhow::Result<()> {
        let app = router(test_state(pool.clone()));

        for path in ["/healthz", "/readyz"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), StatusCode::OK, "{path} must stay public");
        }

        for path in [
            "/api/fleet",
            "/api/me",
            "/api/enrollment-tokens",
            "/api/ca.pem",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty())?)
                .await?;
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "{path} must require a session"
            );
        }
        Ok(())
    }

    /// A different transport (`Sse::new` not `Json`) that looks nothing like
    /// `/api/fleet` -- exactly the kind of route a refactor could
    /// accidentally move outside the `api` sub-router. Asserts the exact
    /// status so a regression (200 or 500) isn't mistaken for a pass.
    #[sqlx::test]
    async fn logs_stream_route_requires_a_session(pool: PgPool) -> anyhow::Result<()> {
        let app = router(test_state(pool));

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{}/logs/stream?source=journal:nginx.service",
                        Uuid::new_v4()
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "the logs SSE stream route is no longer behind auth -- it must 401 \
             before ever opening a tail on the agent, not start streaming"
        );
        Ok(())
    }

    /// A successful upgrade returns `101`, not `200`. `require_auth` wraps
    /// the whole `api` sub-router, so it must reject with `401` BEFORE axum
    /// hands off to `WebSocketUpgrade` -- moving this route outside that
    /// layer would turn it into a live unauthenticated root shell.
    #[sqlx::test]
    async fn terminal_websocket_upgrade_requires_a_session(pool: PgPool) -> anyhow::Result<()> {
        let app = router(test_state(pool));

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/machines/{}/terminal", Uuid::new_v4()))
                    .header("connection", "upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "the terminal route is no longer behind auth -- a WS upgrade \
             attempt with no session must 401, not 101 Switching Protocols"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn an_unknown_or_expired_cookie_is_rejected(pool: PgPool) -> anyhow::Result<()> {
        let app = router(test_state(pool.clone()));

        // Well-formed but unknown token.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header(
                        "cookie",
                        format!("{}=not-a-real-token", argus_common::SESSION_COOKIE),
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // A real session that has expired must also be rejected -- this is the
        // check that proves expiry is server-side, not cookie-age based.
        let (token, hash) = crate::auth::session::new_session_token();
        let id = repo::Identity {
            subject: "s".into(),
            email: None,
            display_name: None,
        };
        repo::create_session(
            &pool,
            &hash,
            &id,
            OffsetDateTime::now_utc() - time::Duration::minutes(1),
        )
        .await?;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header(
                        "cookie",
                        format!("{}={}", argus_common::SESSION_COOKIE, token),
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[sqlx::test]
    async fn a_valid_session_reaches_the_handler(pool: PgPool) -> anyhow::Result<()> {
        let (token, hash) = crate::auth::session::new_session_token();
        let id = repo::Identity {
            subject: "sub-9".into(),
            email: Some("op@example.com".into()),
            display_name: Some("Op".into()),
        };
        repo::create_session(
            &pool,
            &hash,
            &id,
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await?;

        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header(
                        "cookie",
                        format!("{}={}", argus_common::SESSION_COOKIE, token),
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await?;
        let me: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(me["subject"], "sub-9");
        assert_eq!(me["email"], "op@example.com");
        Ok(())
    }

    #[sqlx::test]
    async fn local_login_rejects_bad_credentials_without_setting_a_cookie(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let hash = crate::auth::password::hash_password("the-real-one")?;
        repo::upsert_local_admin(&pool, "admin", &hash).await?;

        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"wrong"}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            res.headers().get("set-cookie").is_none(),
            "a failed login must not set a session cookie"
        );

        // CLAUDE.md's audit rule + design §9: `method` must be explicit on
        // the row, not merely "some audit row got written".
        let audited =
            sqlx::query!("SELECT result, detail FROM audit_log WHERE action = 'auth.denied'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(audited.result.as_deref(), Some("denied"));
        assert_eq!(audited.detail, serde_json::json!({ "method": "local" }));
        Ok(())
    }

    #[sqlx::test]
    async fn local_login_with_correct_credentials_sets_a_working_session(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let hash = crate::auth::password::hash_password("the-real-one")?;
        repo::upsert_local_admin(&pool, "admin", &hash).await?;
        let app = router(test_state(pool.clone()));

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"username":"admin","password":"the-real-one"}"#,
                    ))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res
            .headers()
            .get("set-cookie")
            .expect("session cookie")
            .to_str()?
            .to_string();
        assert!(
            cookie.contains("HttpOnly"),
            "session cookie must be HttpOnly"
        );

        // The session must resolve through the SAME middleware OIDC sessions use.
        let me = app
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header("cookie", cookie.split(';').next().unwrap())
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(me.status(), StatusCode::OK);
        let me_body = axum::body::to_bytes(me.into_body(), 64 * 1024).await?;
        let me_json: serde_json::Value = serde_json::from_slice(&me_body)?;
        // Design §8: `local:` namespaces the subject so it can never collide
        // with a provider's `sub`. Nothing else asserts this at this level --
        // pin it so a refactor that drops the prefix fails here.
        assert_eq!(me_json["subject"], "local:admin");

        // Same audit-detail rationale as the login-denial test above.
        let audited =
            sqlx::query!("SELECT result, detail FROM audit_log WHERE action = 'auth.login'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(audited.result.as_deref(), Some("ok"));
        assert_eq!(audited.detail, serde_json::json!({ "method": "local" }));
        Ok(())
    }

    /// No admin configured must be indistinguishable from a wrong password.
    #[sqlx::test]
    async fn local_login_with_no_admin_configured_returns_the_same_failure(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"anything"}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get("set-cookie").is_none());
        Ok(())
    }

    /// Pins body AND status as byte-identical across all three failure
    /// reasons (wrong password, wrong username, no admin) -- design §11's
    /// whole point is that these cases must be indistinguishable.
    #[sqlx::test]
    async fn local_login_failure_response_is_identical_across_all_three_reasons(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        async fn body_and_status(
            app: Router,
            payload: &str,
        ) -> anyhow::Result<(StatusCode, Vec<u8>)> {
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/auth/local")
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))?,
                )
                .await?;
            let status = res.status();
            let body = to_bytes(res.into_body(), usize::MAX).await?.to_vec();
            Ok((status, body))
        }

        // Case 1: no admin row exists at all.
        let (status_no_admin, body_no_admin) = body_and_status(
            router(test_state(pool.clone())),
            r#"{"username":"admin","password":"whatever"}"#,
        )
        .await?;

        let hash = crate::auth::password::hash_password("the-real-one")?;
        repo::upsert_local_admin(&pool, "admin", &hash).await?;

        // Case 2: a row exists, but the password is wrong.
        let (status_wrong_password, body_wrong_password) = body_and_status(
            router(test_state(pool.clone())),
            r#"{"username":"admin","password":"wrong"}"#,
        )
        .await?;

        // Case 3: a row exists, but the username is wrong.
        let (status_wrong_username, body_wrong_username) = body_and_status(
            router(test_state(pool.clone())),
            r#"{"username":"root","password":"the-real-one"}"#,
        )
        .await?;

        assert_eq!(status_no_admin, StatusCode::UNAUTHORIZED);
        assert_eq!(status_no_admin, status_wrong_password);
        assert_eq!(status_no_admin, status_wrong_username);
        assert_eq!(
            body_no_admin, body_wrong_password,
            "no-admin-configured and wrong-password bodies must be byte-identical"
        );
        assert_eq!(
            body_no_admin, body_wrong_username,
            "no-admin-configured and wrong-username bodies must be byte-identical"
        );

        Ok(())
    }

    #[sqlx::test]
    async fn local_login_returns_429_with_retry_after_before_touching_the_database(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let state = test_state(pool);
        // If the handler queried the DB on this path it would 401/500, never
        // 429. `check` itself reserves the slot it grants, so calling it
        // alone (no `record_failure`) is enough to spend the whole burst.
        let now = std::time::Instant::now();
        for _ in 0..(crate::auth::ratelimit::BURST + 1) {
            state.limiter.check(now);
        }
        let app = router(state);

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"x"}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            res.headers().get("retry-after").is_some(),
            "a 429 must carry Retry-After"
        );
        Ok(())
    }

    /// `check` must reserve the slot it grants, not just report "under
    /// budget" -- otherwise concurrent requests could all read the same
    /// pre-reservation count and all pass. Fires far more than `BURST`
    /// requests SIMULTANEOUSLY (no admin row, so each pays the real ~100ms
    /// dummy-hash path, giving them real overlapping wall-clock time to race).
    #[sqlx::test]
    async fn concurrent_local_logins_cannot_collectively_exceed_the_burst(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let app = router(test_state(pool));
        let total = crate::auth::ratelimit::BURST + 15;

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..total {
            let app = app.clone();
            set.spawn(async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/auth/local")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"username":"admin","password":"x"}"#))
                        .expect("request"),
                )
                .await
                .expect("router call")
                .status()
            });
        }

        let mut unauthorized = 0u32;
        let mut too_many = 0u32;
        while let Some(res) = set.join_next().await {
            match res? {
                StatusCode::UNAUTHORIZED => unauthorized += 1,
                StatusCode::TOO_MANY_REQUESTS => too_many += 1,
                other => panic!("unexpected status {other} from a concurrent local login"),
            }
        }

        assert_eq!(
            unauthorized,
            crate::auth::ratelimit::BURST,
            "concurrency must not let more than BURST requests past the limiter"
        );
        assert_eq!(too_many, total - crate::auth::ratelimit::BURST);

        Ok(())
    }

    #[sqlx::test]
    async fn local_admin_rotate_requires_auth_and_issues_a_new_password(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // A NON-default username: a hardcoded `reset_local_admin(pool,
        // "admin")` -- the shortcut `rotate` deliberately avoids -- would
        // pass this test identically. Only a non-default seed tells them apart.
        let hash = crate::auth::password::hash_password("original")?;
        repo::upsert_local_admin(&pool, "breakglass", &hash).await?;

        // No session: must not rotate anything.
        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/local-admin/rotate")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let cookie = auth_cookie(&pool).await?;
        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/local-admin/rotate")
                    .header("cookie", &cookie)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        let new_password = v["password"].as_str().expect("password field").to_string();
        assert_ne!(new_password, "original");

        let row = repo::get_local_admin(&pool)
            .await?
            .expect("row still present");
        assert_eq!(
            row.username, "breakglass",
            "rotate must not rename the account"
        );
        assert!(crate::auth::password::verify_password(
            &new_password,
            &row.password_hash
        ));

        let audited =
            sqlx::query!("SELECT result FROM audit_log WHERE action = 'local_admin.rotate'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(audited.result.as_deref(), Some("ok"));

        Ok(())
    }

    /// Three PATCH calls, each mutating one thing: tags-only leaves
    /// `display_name` untouched; setting then clearing it with explicit
    /// `null` proves the double-`Option` distinguishes "absent" from "clear".
    #[sqlx::test]
    async fn patch_machine_partial_update_and_audit(pool: PgPool) -> anyhow::Result<()> {
        let id: Uuid = sqlx::query_scalar!(
            r#"INSERT INTO machines (machine_id, hostname, tags) VALUES ('m-1', 'host-1', '{old}')
               RETURNING id"#
        )
        .fetch_one(&pool)
        .await?;
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool.clone()));

        // tags only: display_name must be untouched (null), old tags replaced.
        let res = request_json(
            &app,
            "PATCH",
            &format!("/api/machines/{id}"),
            &cookie,
            serde_json::json!({ "tags": [" Infra ", "media", "infra"] }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.display_name, None);
        assert_eq!(row.tags, vec!["infra", "media"]);

        // display_name set, then cleared with null.
        request_json(
            &app,
            "PATCH",
            &format!("/api/machines/{id}"),
            &cookie,
            serde_json::json!({ "display_name": "Media box" }),
        )
        .await;
        request_json(
            &app,
            "PATCH",
            &format!("/api/machines/{id}"),
            &cookie,
            serde_json::json!({ "display_name": null }),
        )
        .await;
        let dn = sqlx::query_scalar!("SELECT display_name FROM machines WHERE id = $1", id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(dn, None);

        // Audit rows written, naming the changed fields: one per PATCH call
        // above (tags, then display_name, then display_name again to clear
        // it) -- three calls, three mutations, three rows.
        let audits = sqlx::query!(
            r#"SELECT detail FROM audit_log WHERE action = 'machine.update' ORDER BY ts"#
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(audits.len(), 3);
        assert_eq!(audits[0].detail["fields"][0], "tags");
        Ok(())
    }

    /// Every rejection class (bad tag, over-length name, empty body) 400s
    /// and writes nothing. An unknown machine id is well-formed but matches
    /// nothing, so it 404s instead of 400 -- and likewise audits nothing.
    #[sqlx::test]
    async fn patch_machine_rejects_bad_input(pool: PgPool) -> anyhow::Result<()> {
        let id: Uuid = sqlx::query_scalar!(
            r#"INSERT INTO machines (machine_id, hostname) VALUES ('m-2', 'host-2') RETURNING id"#
        )
        .fetch_one(&pool)
        .await?;
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool.clone()));

        for bad in [
            serde_json::json!({ "tags": ["has space"] }),
            serde_json::json!({ "display_name": "x".repeat(65) }),
            serde_json::json!({}),
        ] {
            let res =
                request_json(&app, "PATCH", &format!("/api/machines/{id}"), &cookie, bad).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
        // None of the three 400s above mutated anything, so no audit row.
        let n: i64 = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!" FROM audit_log WHERE action = 'machine.update'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(n, 0);
        // Unknown machine: 404, and still no audit row.
        let res = request_json(
            &app,
            "PATCH",
            &format!("/api/machines/{}", Uuid::new_v4()),
            &cookie,
            serde_json::json!({ "tags": [] }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let n: i64 = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!" FROM audit_log WHERE action = 'machine.update'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(n, 0);
        Ok(())
    }

    /// Fail-closed proof: forces the `machine.update` audit write to fail
    /// while the row it attaches to genuinely exists, then asserts the
    /// mutation didn't survive.
    ///
    /// Can't reuse the FK-violation trick (`patch_machine` uses the SAME id
    /// for both the update and the audit row, so reaching the audit write
    /// requires a real row match -- that FK can never fail here). Instead, a
    /// `BEFORE INSERT` trigger rejects any `machine.update` audit insert --
    /// the same class of injected failure, adapted to this ordering.
    #[sqlx::test]
    async fn patch_machine_rolls_back_the_update_when_the_audit_write_fails(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            CREATE FUNCTION test_reject_machine_update_audit() RETURNS trigger AS $$
            BEGIN
                RAISE EXCEPTION 'injected failure: machine.update audit blocked for test';
            END;
            $$ LANGUAGE plpgsql
            "#
        )
        .execute(&pool)
        .await?;
        sqlx::query!(
            r#"
            CREATE TRIGGER test_reject_machine_update_audit
                BEFORE INSERT ON audit_log
                FOR EACH ROW
                WHEN (NEW.action = 'machine.update')
                EXECUTE FUNCTION test_reject_machine_update_audit()
            "#
        )
        .execute(&pool)
        .await?;

        let id: Uuid = sqlx::query_scalar!(
            r#"INSERT INTO machines (machine_id, hostname, tags) VALUES ('m-3', 'host-3', '{orig}')
               RETURNING id"#
        )
        .fetch_one(&pool)
        .await?;
        let cookie = auth_cookie(&pool).await?;
        let app = router(test_state(pool.clone()));

        let res = request_json(
            &app,
            "PATCH",
            &format!("/api/machines/{id}"),
            &cookie,
            serde_json::json!({ "display_name": "should not stick", "tags": ["new"] }),
        )
        .await;
        assert_eq!(
            res.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a failed audit write must fail closed"
        );

        let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            row.display_name, None,
            "the update must have rolled back with the failed audit write"
        );
        assert_eq!(
            row.tags,
            vec!["orig"],
            "the update must have rolled back with the failed audit write"
        );

        let n: i64 = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!" FROM audit_log WHERE action = 'machine.update'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            n, 0,
            "the rejected audit insert must not have landed either"
        );

        Ok(())
    }
}
