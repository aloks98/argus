//! Browser HTTP surface (PRD §9.1).
//!
//! Sits behind Traefik + cert-manager + Zitadel OIDC. Serves the embedded React
//! app with SPA fallback, plus health endpoints. The `/api`, SSE, and WebSocket
//! routes land with their slices.

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
    routing::{get, post},
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

/// Shared router state: the Postgres pool backing `/api` handlers, the
/// in-memory session `Hub` backing the Docker state + verb endpoints, the
/// OIDC config, the field cipher the OIDC flow uses to seal its pre-auth flow
/// cookie (the same `FieldCipher` that already protects `ca_material`), and
/// the lazily-discovering OIDC client (`crate::auth::oidc`).
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub hub: Arc<Hub>,
    /// `None` when OIDC is not configured (local-admin design §4) -- a valid,
    /// boot-succeeding state. `/auth/login` and `/auth/callback` degrade to a
    /// 404 rather than unwrap it.
    pub oidc: Option<Arc<crate::config::OidcConfig>>,
    pub cipher: Arc<crate::crypto::FieldCipher>,
    pub oidc_client: Option<Arc<crate::auth::oidc::OidcClient>>,
    /// `Config.public_url`, carried independently of `oidc` so the session
    /// cookie's `Secure` attribute can be decided with no OIDC config present
    /// at all -- a local login sets a cookie in exactly that state
    /// (local-admin design §4).
    pub public_url: String,
    /// Guards `POST /auth/local` (local-admin design §10). One instance for
    /// the process's lifetime, shared via this `Arc` clone across every
    /// request -- a per-request limiter would limit nothing.
    pub limiter: Arc<crate::auth::ratelimit::LoginLimiter>,
}

pub async fn serve(cfg: &Config, pool: PgPool, hub: Arc<Hub>) -> Result<()> {
    let oidc = cfg.oidc.clone().map(Arc::new);
    let cipher = Arc::new(
        crate::crypto::FieldCipher::from_b64_key(&cfg.field_key_b64)
            .context("building the field cipher for the OIDC flow cookie")?,
    );
    // Building this is local-only (an HTTP client + an optional local CA cert
    // read); discovery itself happens lazily on first login, never here, so a
    // down IdP at boot cannot delay the agent gRPC surface or health checks.
    // Only built when OIDC is configured at all -- otherwise there is no
    // provider to build a client against.
    let oidc_client = oidc
        .clone()
        .map(crate::auth::oidc::OidcClient::new)
        .transpose()
        .context("building the OIDC client")?
        .map(Arc::new);

    let app = router(AppState {
        pool,
        hub,
        oidc,
        cipher,
        oidc_client,
        public_url: cfg.public_url.clone(),
        limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
    });

    let listener = tokio::net::TcpListener::bind(&cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "browser HTTP surface listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the router without binding a socket, so tests can drive it directly
/// via `tower::ServiceExt::oneshot`.
fn router(state: AppState) -> Router {
    // Everything under /api requires a session -- including the SSE log streams
    // and the terminal WebSocket. Building them as a separate Router and
    // layering once is what guarantees a new /api route cannot be added
    // unprotected by accident.
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
        // `POST /api/local-admin/rotate` lives INSIDE this router deliberately
        // (local-admin design §5.2): it must sit behind the same
        // `require_auth` layer and `SameSite=Lax` cookie as every other verb,
        // not under the public `/auth/*` prefix.
        .route("/api/local-admin/rotate", post(crate::auth::local::rotate))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_auth,
        ));

    // Public: infra endpoints (PRD §9.1 puts them outside OIDC), the OIDC flow
    // itself, the local-admin login, and the SPA bundle -- which must render
    // the sign-in view BEFORE any session exists. The bundle is an empty
    // shell; all data is behind /api.
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/auth/login", get(crate::auth::oidc::login))
        .route("/auth/callback", get(crate::auth::oidc::callback))
        .route("/auth/logout", post(crate::auth::oidc::logout))
        .route("/auth/local", post(crate::auth::local::login))
        .merge(api)
        .fallback(static_handler)
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

async fn readyz() -> impl IntoResponse {
    // TODO(spine): gate on Postgres connectivity so we don't accept agents before
    // migrations finish (PRD §2.5).
    (StatusCode::OK, "ready")
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
    /// Hub snapshot. `0` when nothing has ever been reported.
    ///
    /// Snapshots are NOT evicted when an agent disconnects (`Hub::unregister`
    /// only clears the connection registry), so for an offline machine this is
    /// the last known value and may be arbitrarily stale — a machine that goes
    /// offline with 3 failed units keeps reporting 3. The row pairs it with the
    /// machine's own `status`, so the staleness is visible rather than implied,
    /// but do not read this as a live count for a machine that isn't `online`.
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

/// `GET /api/fleet` -- list every machine with its status, for the fleet page.
/// Behind `require_auth` (mounted once on the whole `/api` router).
async fn fleet(State(state): State<AppState>) -> Result<Json<Vec<FleetRow>>, StatusCode> {
    let rows = sqlx::query!(
        r#"SELECT id, hostname, display_name, os, host(primary_ip) as "primary_ip?", status,
                  last_seen_at, tags, capabilities FROM machines ORDER BY hostname"#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "failed to list fleet");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let spark_rows = repo::recent_series_all(&state.pool, 20)
        .await
        .map_err(|err| {
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
        }
    }
}

/// `GET /api/machines/{id}` -- one machine's full inventory row, for the
/// detail page.
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

fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Some(Option::<String>::deserialize(d)?))
}

/// `PATCH /api/machines/{id}` -- display name / notes / tags. Only the fields
/// present in the body change. Every outcome that MUTATED is audited; a 400
/// mutates nothing and is not.
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

    // Fail closed (CLAUDE.md: every verb goes through the audit log from the
    // start): the mutation and its `machine.update` row are ONE transaction,
    // committed together or not at all. `run_verb` gets fail-closed by
    // auditing BEFORE dispatch, but that ordering isn't available here --
    // "does this machine exist" (the 404) can only be answered by attempting
    // the update itself, so the update necessarily runs first. Sharing a `tx`
    // instead means a failed audit write rolls the update back with it: an
    // unaudited mutation must never persist, and a client told "500" must
    // find the database agrees.
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
        // `tx` drops here un-committed -- there was nothing to roll back (no
        // row matched), but this keeps the "never commit without an audit
        // row" invariant true unconditionally rather than by accident.
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
        // `tx` drops here un-committed, which rolls the update back: the
        // whole point of sharing a transaction with the write above.
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

/// `GET /api/machines/{id}/metrics?range=` -- a machine's metrics history for
/// the detail-page charts. `range` is one of `1h` (default), `6h`, `24h`.
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

/// `GET /api/machines/{id}/docker` — the machine's latest cached container list
/// (empty when the agent hasn't reported / has no Docker daemon).
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

/// `GET /api/machines/{id}/systemd` — the machine's latest cached unit list
/// (empty when the agent hasn't reported / has no systemd).
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

/// Resolve the UI's single `window` value plus `priority` into concrete filters.
/// `window` is one of `boot | 1h | 24h | all`; `boot` and a relative window are
/// alternative answers to the same question and are never combined.
///
/// `since_ms` is resolved to an ABSOLUTE epoch here, but freshly — from `now`
/// at the moment THIS request is handled, not anchored to when the view was
/// first opened. This function is called independently for the tail request
/// and for every later page request of the same view, so each read is
/// self-consistent with the window as of THAT request, but the window itself
/// creeps forward across the view's lifetime rather than staying pinned. A
/// view left open across a window boundary — e.g. `window=24h` held open for
/// more than a day — can therefore end up holding lines older than its own
/// (current) window. Returns `None` when the input is invalid, which the
/// caller turns into a 400.
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

/// `resolve_log_filters` plus an explicit, already-resolved cutoff.
///
/// A page read must use the SAME cutoff its stream was given, otherwise every
/// request re-resolves `now` and a view open longer than its own window finds
/// each page fully truncated — reporting "beginning of window" while still
/// displaying a longer span. The stream announces its resolved cutoff in a
/// `meta` frame and the client echoes it here, so `since_ms` wins over `window`.
/// `window` is still validated even when overridden, so a malformed request is
/// rejected rather than silently accepted.
///
/// `Some(0)` is treated as "no explicit cutoff", matching the convention used
/// everywhere else in this codebase that `since_ms == 0` means unset (see
/// `crates/agent/src/logs.rs`). This is not a hypothetical: `window=boot`
/// resolves to `since_ms = 0`, so the `meta` frame announces `{"since_ms":0}`
/// and the client dutifully echoes `since_ms=0` back on every page read of a
/// boot-windowed tail. Treating that as authoritative would set
/// `current_boot = false` and silently drop the boot filter on every page
/// read — the resolved `since_ms=0` cutoff is then inert (0 means unset), so
/// the page comes back unfiltered by boot at all.
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

/// Filters only apply to journal reads — `run_docker` ignores them entirely,
/// so a docker source has no priority or window concept. Zero them for any
/// non-journal source before they're forwarded to the agent or recorded in the
/// audit target, so what's dispatched (and what's audited) matches what the
/// agent actually does with the read rather than asserting a filter that was
/// silently ignored.
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

/// Sends `LogTailStop` when the SSE response is dropped — i.e. the browser
/// navigated away, closed the tab, or lost its connection. This is the only
/// thing that stops a `journalctl -f` from outliving the view that asked for
/// it, so it must stay owned by the stream.
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
    // Docker ignores filters entirely; zero them before dispatch and audit so
    // neither claims a narrowing that never happened.
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

    // Reading logs is not a mutation, but it can expose secrets, so who read
    // what is recorded — the PRD already treats terminal.open the same way.
    // There is no result to update later, so the row is written once as `ok`,
    // only now that the tail is actually live on the agent.
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

    // Announce the resolved cutoff FIRST, so every page read for this tail can
    // echo it back instead of re-resolving `now` and drifting. A *named* event
    // deliberately: the browser's EventSource routes named events to
    // addEventListener and NOT to onmessage, so this frame cannot be mistaken
    // for a log line by the existing NDJSON parsing.
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

/// `GET /api/machines/{id}/logs/page?source=&before=&limit=` — one backward page
/// of a unit's journal, collected from a short-lived non-follow tail. Journal
/// only; docker has no cursor.
async fn logs_page(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<LogPageQuery>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    // Journal only: reuse the shared validator, then reject anything but a
    // journal source (docker paging is unsupported).
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

    // Open the tail and dispatch BEFORE auditing, mirroring log_stream: an
    // offline agent must 409 with NO `logs.page` row at all, and a later
    // timeout must not leave an `ok` row for a read that never returned.
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

    // Reading logs is not a mutation but can expose secrets, so record who read
    // what — only now that the page read is actually live on the agent. Same
    // posture as logs.open; fail closed if the row can't be written.
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

    // Collect chunks until eof (or timeout). The agent sends the whole page then
    // an eof chunk.
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
    // Tear the short-lived tail down on the agent too. The page read is a
    // non-follow process that exits on its own, but the agent only drops its
    // AbortHandle map entry on LogTailStop, so without this each page fetch
    // would leak a dead handle for the session's lifetime.
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

/// `POST /api/machines/{id}/docker/{container}/{action}` — dispatch a container
/// verb and wait up to `VERB_TIMEOUT` for the agent's result.
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

/// `POST /api/machines/{id}/units/{unit}/{action}` — dispatch a systemd unit
/// verb and wait up to `VERB_TIMEOUT` for the agent's result.
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
    // A systemd unit name is never empty and never contains '/'. Reject rather
    // than forward something an agent would only fail on anyway. (The empty case
    // is unreachable through the router — matchit won't bind an empty path
    // segment — but is kept so the guard holds for any direct caller.)
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

/// The shared verb pipeline for every verb family: audit-before-dispatch (fail
/// closed), dispatch, then a bounded wait for the agent's result. `timeout` is
/// injected so tests don't wait the full 10s. `identity` is the caller
/// authenticated by `require_auth` -- it attributes both the audit row and the
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

    // Register the waiter AND write the dispatched audit row BEFORE dispatch, so
    // the row is guaranteed to exist before the agent can round-trip a
    // CommandResult -- whose grpc-side UPDATE (repo::update_command_result) is
    // keyed by command_id and would otherwise silently no-op against a
    // not-yet-inserted row, freezing it at "dispatched" forever.
    //
    // NOTE: "dispatched" is NOT a guaranteed-terminal state. The agent runs a
    // verb in a spawned task holding a clone of the *current* session's sender;
    // if that session ends before the job finishes -- and a unit verb may now
    // take up to 90s -- the CommandResult send is dropped and nothing ever
    // resolves this row. Reconciling stale `dispatched` rows to `unknown` is a
    // scheduled, survives-restart job, which CLAUDE.md gates behind the
    // apalis-vs-pgmq validation, so it is deliberately deferred rather than
    // hand-rolled here. Do not assume a row's result is final.
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
        // The agent is offline: no CommandResult will ever arrive to resolve the
        // row, so flip it to the terminal "denied" state here. This is the one
        // case the grpc CommandResult arm cannot cover (the command was never
        // delivered), so it does not conflict with that arm being the sole
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
        Some(content) => (
            [(header::CONTENT_TYPE, content_type(path))],
            content.data.into_owned(),
        )
            .into_response(),
        None => match Assets::get("index.html") {
            Some(index) => (
                [(header::CONTENT_TYPE, "text/html")],
                index.data.into_owned(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
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

    /// `repo`-level tests already pin the tri-state through `upsert_machine`
    /// and `machine_detail`; this pins the SAME tri-state through the actual
    /// `GET /api/machines/{id}` JSON body, which is the exact contract the
    /// frontend's tab-gating reads (`MachineDetailPage.tsx`'s `caps` /
    /// `lacks()`). `null` and `[]` are not interchangeable: `null` must gate
    /// NOTHING (an agent that predates capability reporting) and `[]` must
    /// gate EVERYTHING (a bare host that reported and has none) -- so the
    /// distinction has to survive serialization, not just the repo layer.
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

    fn app_state_with_hub(pool: PgPool) -> (AppState, Arc<Hub>) {
        let hub = Arc::new(Hub::new());
        let mut state = test_state(pool);
        state.hub = hub.clone();
        (state, hub)
    }

    /// A stand-in `Identity` for tests that call `run_verb`/`open_and_audit`
    /// directly rather than through the router (so they never touch
    /// `require_auth`, but the handlers themselves now take an `Identity`).
    fn test_identity() -> repo::Identity {
        repo::Identity {
            subject: "test-subject".into(),
            email: Some("test@example.com".into()),
            display_name: None,
        }
    }

    /// Seeds a live session and returns a `Cookie` header value, for tests
    /// that drive `/api` routes through the router: every one of them now
    /// sits behind `require_auth`, so a request with no cookie gets a 401
    /// before ever reaching the handler under test.
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

    /// Drives an authenticated JSON request straight through the router --
    /// method, uri, the session cookie, and a serialized JSON body -- for the
    /// handlers that take a body (only `PATCH /api/machines/{id}` so far).
    /// Mirrors the file's existing `Request::builder()...oneshot()` style
    /// used throughout this module, just consolidated so PATCH tests don't
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

        // A unit name is never empty and never contains '/'. `%2F` decodes to a
        // slash inside the path segment, which must not be forwarded to an agent.
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

        // The connectivity failure must be checked BEFORE the audit write: a
        // 409 must never leave a misleading `logs.open`/`ok` row behind for a
        // read that never actually opened.
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

        // The leading frame must be a NAMED `meta` event: a browser's
        // EventSource routes named events to addEventListener and NOT to
        // onmessage, so an unnamed frame would be parsed as a log line by the
        // client's NDJSON code. And it must come BEFORE the log chunk, so a
        // page read never sees a chunk before it knows what cutoff to echo
        // back.
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
                    // A docker read carries no filters, whatever the query
                    // string asked for: `run_docker` ignores them entirely.
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

    // --- resolve_log_filters: the ONLY definition of the `window` -> proto
    // mapping (see the doc comment on the function). Direct, DB-free coverage
    // so swapping the 3_600_000 / 86_400_000 constants can't slip past every
    // other gate silently.

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
        // window="boot" alone sets current_boot=true (see
        // resolve_log_filters_boot_sets_current_boot_and_no_since below). An
        // explicit since_ms must still win and flip it back to false: an
        // explicit cutoff and a boot window are alternative answers to the
        // same question and must never combine. This is the true->false
        // transition; `explicit_since_ms_overrides_the_window` above uses
        // window="1h", whose current_boot is already false, so it never
        // exercises this override.
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
        // Unlike `explicit_since_ms_turns_off_a_requested_boot_window` above
        // (window=boot + since_ms=1_600_000_000_000, a pair the client can
        // never actually produce), this is the pair that IS reachable
        // end-to-end: `resolve_log_filters(_, Some("boot"))` resolves to
        // `since_ms = 0`, so the `meta` frame announces `{"since_ms":0}` and
        // the client echoes `since_ms=0` back on every subsequent page read
        // of that tail. `since_ms == 0` means "unset" everywhere else in this
        // codebase (crates/agent/src/logs.rs), so it must NOT be treated as
        // an authoritative cutoff here either — doing so would flip
        // `current_boot` to false and silently drop the boot filter on every
        // page read.
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

        for path in ["/api/fleet", "/api/me"] {
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

    /// The SSE log stream is a different transport from the plain JSON
    /// handlers above (`Sse::new(...).into_response()` rather than
    /// `Json(...)`), and it's exactly the kind of route a future refactor
    /// could accidentally move outside the `api` sub-router without anyone
    /// noticing -- nothing about it visually resembles `/api/fleet`. Assert
    /// the exact status so a regression that turned this into a 200 (started
    /// streaming) or a 500 wouldn't be mistaken for a pass.
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

    /// The terminal WebSocket is the other transport that differs from a
    /// plain JSON handler: a successful upgrade returns `101 Switching
    /// Protocols`, not `200`. `require_auth` runs as a `Router` layer wrapping
    /// the whole `api` sub-router, so it must reject the upgrade with `401`
    /// BEFORE axum ever hands the connection to `WebSocketUpgrade` -- a
    /// refactor that moved `/api/machines/{id}/terminal` outside that layer
    /// (or reordered the layer past the WS route) would turn this into a live
    /// unauthenticated root shell. `oneshot` never actually completes the
    /// upgrade (there's no live socket on either end), so asserting the
    /// response status here is sufficient to prove the layer ran first.
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

        // CLAUDE.md: every verb goes through the audit log from the start.
        // Design §9 requires `method` on the row explicitly, not merely "some
        // audit row got written".
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
        // Design §8: the `local:` prefix namespaces the subject so it can
        // never collide with a provider's `sub`. Nothing else asserts this --
        // pin it so a future refactor that drops or reshapes the prefix
        // fails here instead of silently opening a collision path.
        assert_eq!(me_json["subject"], "local:admin");

        // CLAUDE.md: every verb goes through the audit log from the start,
        // and design §9 requires `method` be explicit on the row -- nothing
        // else would notice it silently vanishing.
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

    /// Same failure across all three reasons: wrong password, wrong username
    /// (a row exists, but the caller names a different one), and no admin
    /// configured at all. Pins body AND status as byte-identical, not merely
    /// "some 401" -- design §11's whole point is that these three cases must
    /// be indistinguishable to the caller.
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
        // Exhaust the burst directly against the limiter -- no admin row is
        // ever created, so if the handler queried the database on this path
        // it would 401 (or 500, absent a row/table setup), never 429. `check`
        // itself reserves the slot it grants (the concurrency fix), so
        // calling it alone -- with no separate `record_failure` -- is enough
        // to spend the whole burst.
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

    /// The concurrency bug this fix round exists for, proven at the real
    /// router: fire far more than `BURST` requests at `/auth/local`
    /// SIMULTANEOUSLY (no admin row configured, so every one of them takes
    /// the real, ~100ms argon2 dummy-hash path -- giving the verifies actual
    /// overlapping wall-clock time to race in). Under the old
    /// check-then-record-failure-later design, every one of them would read
    /// "under budget" before any recorded itself, so all would pass and none
    /// would ever see a 429. With the reservation folded into `check`, at
    /// most `BURST` may ever be admitted regardless of how they interleave.
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
        // A NON-default username: a hardcoded `reset_local_admin(&pool, "admin")`
        // -- the exact shortcut `rotate` deliberately avoids -- would pass a
        // test seeded with "admin" identically to the real username-preserving
        // implementation. Only a non-default seed can tell the two apart.
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

    /// Three PATCH calls, each mutating exactly one thing, each producing its
    /// own `machine.update` audit row: tags-only leaves `display_name`
    /// untouched; setting `display_name` then clearing it with an explicit
    /// `null` proves the double-`Option` distinguishes "absent" from "clear".
    #[sqlx::test]
    async fn patch_machine_partial_update_and_audit(pool: PgPool) -> anyhow::Result<()> {
        // Seed a machine directly (raw SQL precondition, per the standing lesson).
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

    /// Every rejection class (bad tag, over-length display name, empty body)
    /// returns 400 and writes nothing -- neither to `machines` nor
    /// `audit_log`. An unknown machine id is a well-formed request that
    /// simply doesn't match anything, so it 404s instead of 400 and likewise
    /// audits nothing.
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

    /// Fail-closed proof for the transactional fix (review round 1): forces
    /// the `machine.update` audit write to fail while the machine row it
    /// would attach to genuinely exists, then asserts the mutation did not
    /// survive that failure.
    ///
    /// This can't reuse `verb_fails_closed_when_the_dispatched_audit_write_fails`'s
    /// FK-violation trick verbatim (a "ghost" `machine_id` with no `machines`
    /// row, so the audit insert's FK fails): `patch_machine` uses the SAME
    /// `id` for both the update and the audit row, and reaching the audit
    /// write at all requires the update to have matched a real row first --
    /// so by construction that FK can never be what fails here. Instead, a
    /// `BEFORE INSERT` trigger scoped to this test's own isolated database
    /// rejects any `audit_log` insert whose `action = 'machine.update'`,
    /// which fails the write for a reason orthogonal to whether the
    /// referenced machine exists -- the same *class* of injected DB-level
    /// failure, adapted to an ordering where the FK approach doesn't apply.
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
