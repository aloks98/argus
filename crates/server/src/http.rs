//! Browser HTTP surface (PRD §9.1).
//!
//! Sits behind Traefik + cert-manager + Zitadel OIDC. Serves the embedded React
//! app with SPA fallback, plus health endpoints. The `/api`, SSE, and WebSocket
//! routes land with their slices.

use crate::config::Config;
use crate::embed::Assets;
use crate::hub::{DispatchError, Hub};
use crate::repo;
use anyhow::Result;
use argus_proto::v1::Verb;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

/// Shared router state: the Postgres pool backing `/api` handlers, plus the
/// in-memory session `Hub` backing the Docker state + verb endpoints.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub hub: Arc<Hub>,
}

pub async fn serve(cfg: &Config, pool: PgPool, hub: Arc<Hub>) -> Result<()> {
    let app = router(AppState { pool, hub });

    let listener = tokio::net::TcpListener::bind(&cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "browser HTTP surface listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the router without binding a socket, so tests can drive it directly
/// via `tower::ServiceExt::oneshot`.
fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/api/fleet", get(fleet))
        .route("/api/machines/{id}", get(machine))
        .route("/api/machines/{id}/metrics", get(machine_metrics))
        .route("/api/machines/{id}/docker", get(machine_docker))
        .route(
            "/api/machines/{id}/docker/{container}/{action}",
            post(container_action),
        )
        // TODO: nest remaining /api routes (logs SSE, terminal WS,
        // events SSE, audit, enroll-tokens) and /auth OIDC routes here (PRD
        // §9.1).
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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
    os: Option<String>,
    primary_ip: Option<String>,
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    last_seen_at: Option<OffsetDateTime>,
    tags: Vec<String>,
    cpu_pct: Option<f32>,
    mem_pct: Option<f64>,
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
/// Intentionally UNAUTHENTICATED for the Spine slice: OIDC lands later, once
/// the browser surface moves behind Traefik + Zitadel (PRD §9.1).
async fn fleet(State(state): State<AppState>) -> Result<Json<Vec<FleetRow>>, StatusCode> {
    let rows = sqlx::query!(
        r#"SELECT id, hostname, os, host(primary_ip) as "primary_ip?", status,
                  last_seen_at, tags FROM machines ORDER BY hostname"#
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

            FleetRow {
                id: r.id,
                hostname: r.hostname,
                os: r.os,
                primary_ip: r.primary_ip,
                status: r.status,
                last_seen_at: r.last_seen_at,
                tags: r.tags,
                cpu_pct,
                mem_pct,
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
}

impl From<repo::MachineDetail> for MachineDetailDto {
    fn from(d: repo::MachineDetail) -> Self {
        MachineDetailDto {
            id: d.id,
            machine_id: d.machine_id,
            hostname: d.hostname,
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
) -> Response {
    run_container_verb(&state, id, &container, &action, VERB_TIMEOUT).await
}

/// Testable core (timeout injected so tests don't wait the full 10s).
async fn run_container_verb(
    state: &AppState,
    id: Uuid,
    container: &str,
    action: &str,
    timeout: Duration,
) -> Response {
    let verb = match action {
        "start" => Verb::ContainerStart,
        "stop" => Verb::ContainerStop,
        "restart" => Verb::ContainerRestart,
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    let actor = "anonymous";
    let audit_action = format!("container.{action}");
    let command_id = Uuid::new_v4();
    let cid = command_id.to_string();

    // Register the waiter AND write the dispatched audit row BEFORE dispatch, so
    // the row is guaranteed to exist before the agent can round-trip a
    // CommandResult -- whose grpc-side UPDATE (repo::update_command_result) is
    // keyed by command_id and would otherwise silently no-op against a
    // not-yet-inserted row, freezing it at "dispatched" forever.
    let rx = state.hub.register_pending(cid.clone(), id);
    if let Err(e) = repo::audit_command(
        &state.pool,
        actor,
        &audit_action,
        Some(id),
        container,
        command_id,
        "dispatched",
    )
    .await
    {
        // Fail closed: a verb must never execute unaudited (CLAUDE.md). If the
        // dispatched audit write fails, abandon the waiter and do NOT dispatch.
        state.hub.abandon_pending(&cid);
        tracing::error!(error = %e, "container verb: dispatched audit write failed; not dispatching");
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
            container.to_string(),
            actor.to_string(),
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
            tracing::error!(error = %e, "container verb: denied audit update failed");
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

        let app = router(AppState {
            pool,
            hub: Arc::new(Hub::new()),
        });

        let response = app
            .oneshot(Request::builder().uri("/api/fleet").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["hostname"], "a-online-host");
        assert_eq!(rows[0]["status"], "online");
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

        let app = router(AppState {
            pool,
            hub: Arc::new(Hub::new()),
        });

        // /api/fleet: the seeded machine's row must carry a non-empty
        // spark_cpu ending in the latest sample.
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/api/fleet").body(Body::empty())?)
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
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    fn app_state_with_hub(pool: PgPool) -> (AppState, Arc<Hub>) {
        let hub = Arc::new(Hub::new());
        (
            AppState {
                pool,
                hub: hub.clone(),
            },
            hub,
        )
    }

    #[sqlx::test]
    async fn get_docker_returns_cached_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let (state, hub) = app_state_with_hub(pool);
        let id = Uuid::new_v4();

        // empty before any report
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/docker"))
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
        let resp = run_container_verb(
            &state,
            machine_id,
            "web",
            "restart",
            Duration::from_millis(200),
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

        let resp =
            run_container_verb(&state, machine_id, "web", "start", Duration::from_secs(5)).await;
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

        let resp = run_container_verb(
            &state,
            machine_id,
            "web",
            "stop",
            Duration::from_millis(150),
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

        let resp =
            run_container_verb(&state, ghost_id, "web", "start", Duration::from_millis(200)).await;
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
    async fn verb_with_unknown_action_returns_400(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let resp = run_container_verb(
            &state,
            Uuid::new_v4(),
            "web",
            "obliterate",
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }
}
