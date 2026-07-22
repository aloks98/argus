# Metrics slice — design

Build slice #2 (PRD §"Build order"). Streams host metrics from the agent over the
existing `Session`, stores them in the boring `metrics` table, and surfaces them
two ways: per-row sparklines on the fleet grid and full-history charts on a new
machine-detail page.

## Goal
`sysinfo` on the agent → a `MetricsSample` every 15s over the existing mTLS
`Session` → the control plane inserts a row and refreshes liveness → the browser
shows current values + sparklines (fleet grid) and full charts (detail page).

## Non-goals (deferred to later slices)
- `extra_json` payload: per-disk, per-net, temps, ZFS ARC, S.M.A.R.T. (core scalars
  only for now).
- Per-container stats (that's the Docker slice).
- SSE live push (`/metrics/stream`) — the fleet page's existing 5s poll is enough;
  the detail page polls its range endpoint.
- OIDC/auth on the browser surface (still Spine-deferred).
- apalis for the prune — stays a `tokio` task per the background-work rule.

## Data flow
```
agent sysinfo sampler --(MetricsSample every 15s)--> Session stream
  --> server handle_agent_frame: repo::insert_metrics(..) + repo::touch_last_seen(..)
      --> metrics table (BRIN(ts), already migrated in 0002_metrics.sql)
          --> GET /api/fleet            (latest + short recent series -> grid sparklines)
          --> GET /api/machines/:id/metrics?range  (full series -> detail charts)
```
A `MetricsSample` doubles as a liveness signal (touches `last_seen`); the bare
`Heartbeat` remains the fallback for the window before the first sample.

## Agent — `crates/agent/src/metrics.rs` (new)
Adds the `sysinfo` dependency (this slice's per-slice dep; keep the agent lean).

`fn sample(sys: &mut sysinfo::System, nets: &mut sysinfo::Networks, disks: &mut sysinfo::Disks) -> argus_proto::v1::MetricsSample`, mapping:
- `cpu_pct` ← `sys.global_cpu_usage()` (needs two refreshes ≥ `MINIMUM_CPU_UPDATE_INTERVAL` apart; the 15s tick provides the gap).
- `mem_used`/`mem_total` ← `sys.used_memory()`/`sys.total_memory()`; `swap_used`/`swap_total` likewise.
- `load1/5/15` ← `sysinfo::System::load_average()` (Linux).
- `disk_used`/`disk_total` ← summed across `disks` (total − available, and total).
- `net_rx_bytes`/`net_tx_bytes` ← summed `total_received()`/`total_transmitted()` across interfaces (CUMULATIVE — the proto comment says deltas are computed control-plane-side).
- `uptime_secs` ← `sysinfo::System::uptime()`; `unix_ms` ← wall clock.
- `extra_json` ← `""` for now.

The session sender (`session.rs`) sends one `AgentFrame{ metrics: sample() }` on
each 15s tick (the existing heartbeat interval), keeping the `Hello` snapshot on
connect. sysinfo objects are created once per connection and refreshed per tick.

## Server
`handle_agent_frame` gains a `MetricsSample` arm: `repo::insert_metrics(pool, machine_id, &sample)` then `repo::touch_last_seen(pool, machine_id)`.

New `repo` functions (all `impl PgExecutor`):
- `insert_metrics(exec, machine_id: Uuid, s: &MetricsSampleRow) -> Result<()>` — one INSERT into `metrics` (map proto → columns; store `extra` as `'{}'::jsonb`).
- `recent_series(exec, machine_id: Uuid, limit: i64) -> Result<Vec<Spark>>` where `Spark { ts, cpu_pct, mem_pct }` — the last `limit` samples ascending, for grid sparklines. `mem_pct` is **computed** in SQL (`100 * mem_used / NULLIF(mem_total,0)`), not a stored column.
- `metrics_history(exec, machine_id: Uuid, since: OffsetDateTime) -> Result<Vec<MetricPoint>>` — full samples since `since`, ascending, for detail charts.
- `machine_detail(exec, id: Uuid) -> Result<Option<MachineDetail>>` — inventory + status + tags + notes.
- `latest_metric(exec, machine_id) -> Option<Spark>` (or folded into the fleet query).

`MetricsSampleRow` is a small server-side struct mirroring the proto sample (avoids leaking proto types into `repo`).

## API (browser surface)
- `GET /api/fleet` → each row gains `cpu_pct: number|null`, `mem_pct: number|null` (latest), and `spark: { cpu: number[], mem: number[] }` (last ~20 samples, oldest→newest). Implemented with a lateral/`DISTINCT ON`-style query or a per-row `recent_series`; keep the payload compact.
- `GET /api/machines/:id` → `{ id, machine_id, hostname, os, kernel, arch, primary_ip, agent_version, status, last_seen_at, enrolled_at, tags, notes, latest: MetricPoint|null }`. 404 if unknown.
- `GET /api/machines/:id/metrics?range=1h` → `MetricPoint[]` ascending, where `MetricPoint { ts, cpu_pct, mem_used, mem_total, swap_used, swap_total, load1, disk_used, disk_total, net_rx_bytes, net_tx_bytes }`. `range` ∈ {`1h`,`6h`,`24h`} (default `1h`, reject others with 400). `ts` serialized RFC-3339.

Net is stored cumulative; the **frontend** derives rate (Δbytes / Δt) for display.

## Retention — `jobs.rs`
A second `tokio` task alongside the offline sweeper: every 1h,
`DELETE FROM metrics WHERE ts < now() - (interval from METRICS_RETENTION_HOURS)`,
logging the deleted count. `main.rs` adds it to the `try_join!`. (Hourly is
functionally the PRD's "nightly prune"; no cron/apalis needed.)

## Frontend
Add **react-router-dom**; routes: `/` → `FleetPage`, `/machines/:id` → `MachineDetailPage`. `main.tsx` wraps `<App/>` in a router.

- `api.ts`: extend `FleetRow` with `cpu_pct`/`mem_pct`/`spark`; add `getMachine(id)` and `getMetrics(id, range)` + their types.
- `FleetPage.tsx`: each row shows current cpu%/mem% (small bars/text) + two **inline-SVG mini-sparklines** (cpu, mem) drawn from `spark` — no chart lib, cheap. Hostname links to `/machines/:id`.
- `MachineDetailPage.tsx` (new): header (hostname/os/ip/status/tags/last-seen) + a range selector (1h/6h/24h, polled every ~10s) + charts for cpu%, mem%, load, and net **rate** (derived from deltas), using rnui's chart component (it bundles echarts/recharts — confirm the export at build time; fall back to a plain SVG line if needed). Loading/error/empty states.
- A tiny `Sparkline` component (inline SVG, viewBox-scaled polyline) reused by the grid.

## Testing
- Agent: `metrics::sample` unit test — CPU%/mem/uptime are populated and within sane bounds on the test host.
- Server: `#[sqlx::test]` for `insert_metrics` + `recent_series` + `metrics_history` (seed samples, assert order/shape); extend the `handle_agent_frame` seam test to send a `MetricsSample` and assert a `metrics` row exists and `last_seen_at` advanced.
- HTTP: `oneshot` tests — `/api/fleet` includes `spark`; `/api/machines/:id` returns detail (404 when unknown); `/api/machines/:id/metrics?range=1h` returns ascending points and rejects a bad `range` with 400.
- Frontend: `npm run typecheck` + `build` clean.
- E2E (extends `docs/DEV.md`): a live agent streams metrics → fleet grid sparklines populate within ~1 min → the detail page renders charts; confirm the prune deletes rows older than the window.

## Confirmed defaults
15s sample cadence (`DEFAULT_METRICS_SECS`); 48h retention (`METRICS_RETENTION_HOURS`); hourly `tokio` prune; `react-router-dom`; inline-SVG grid sparklines + rnui charts on the detail page; core scalars only (`extra_json` empty).
