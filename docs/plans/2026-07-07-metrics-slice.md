# Metrics Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stream host metrics from the agent over the existing mTLS `Session`, store them in the `metrics` table, and surface them as per-row sparklines on the fleet grid plus full-history charts on a new machine-detail page.

**Architecture:** The agent samples host stats with `sysinfo` every 15s and sends a `MetricsSample` frame over the already-open `Session`. The server's `handle_agent_frame` inserts a row (server-timestamped) and touches `last_seen`. Three browser endpoints read it back: `/api/fleet` gains latest cpu%/mem% + a short recent series; `/api/machines/:id` returns detail; `/api/machines/:id/metrics?range=` returns a time series. A React detail page (react-router) charts it; the fleet grid draws inline-SVG sparklines. An hourly `tokio` task prunes rows older than 48h.

**Tech Stack:** Rust workspace (`sysinfo` new on the agent); `sqlx` (compile-time-checked); `axum`; React + Vite + `@e412/rnui-react` + `react-router-dom`; design of record `docs/designs/2026-07-07-metrics-slice.md`.

## Global Constraints
- **Dev env is already set up** (from the Spine): dev Postgres runs as docker container `argus-pg`; `DATABASE_URL` is in `.cargo/config.toml [env]`; `sqlx-cli` is installed; git identity + SSH signing are configured; work happens on branch `metrics-slice` (already checked out).
- **sqlx is compile-time-checked.** After adding/changing any `query!`/`query_as!`, run `cargo sqlx prepare --workspace -- --all-targets` and `git add .sqlx`. CI runs `cargo sqlx prepare --check`.
- **CI enforces `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` (offline), and the full test suite.** Keep all three green — run `cargo fmt --all` before committing and check clippy, not just `cargo build` warnings.
- **Every DB write goes through a `repo` function.** No inline SQL in handlers/agent.
- **Agent stays lean:** `sysinfo` is this slice's only new agent dependency.
- **Metrics `ts` = server `now()` at insert** (avoid agent clock skew). Net counters are stored **cumulative**; rate is derived in the frontend from sample deltas.
- **Cadence 15s** (`argus_common::DEFAULT_METRICS_SECS`); **retention 48h** (`argus_common::METRICS_RETENTION_HOURS`); prune is an **hourly `tokio` task** (apalis stays deferred).
- **Numeric mapping:** proto `MetricsSample` uses `u64` for mem/swap/disk/net; the `metrics` columns are `bigint` (`i64`) — cast `as i64` (byte counts never approach `i64::MAX`). `cpu_pct`/`load*` are `real` (`f32`).
- Scope fence: core scalars only (`extra` stays `'{}'`); no `extra_json`/temps/ZFS/per-container/SMART, no SSE, no auth — all deferred.

---

## File structure

```
crates/server/src/
  repo.rs        MOD  insert_metrics, recent_series_all, metrics_history, machine_detail (+ row structs)
  grpc.rs        MOD  handle_agent_frame: MetricsSample arm
  http.rs        MOD  /api/fleet (add cpu/mem/spark), GET /api/machines/:id, GET /api/machines/:id/metrics
  jobs.rs        MOD  hourly metrics prune task
  main.rs        MOD  wire the prune task into try_join!
crates/agent/src/
  metrics.rs     NEW  sysinfo sampler -> MetricsSample
  session.rs     MOD  send a MetricsSample each 15s tick
  Cargo.toml     MOD  add sysinfo
frontend/
  package.json          MOD  add react-router-dom
  src/main.tsx          MOD  wrap App in a BrowserRouter
  src/App.tsx           MOD  <Routes>: / -> FleetPage, /machines/:id -> MachineDetailPage
  src/api.ts            MOD  extend FleetRow; add getMachine, getMetrics + types
  src/Sparkline.tsx     NEW  inline-SVG sparkline
  src/FleetPage.tsx     MOD  current cpu/mem + sparklines + link to detail
  src/MachineDetailPage.tsx  NEW  header + range selector + charts
docs/DEV.md      MOD  metrics E2E notes (Task 10)
Cargo.toml       MOD  add sysinfo to [workspace.dependencies]
```

---

## Task 1: Server repo — metrics persistence + read queries

**Files:** Modify `crates/server/src/repo.rs`.

**Interfaces (Produces):**
- `pub struct MetricsSampleRow { pub cpu_pct: f32, pub mem_used: i64, pub mem_total: i64, pub swap_used: i64, pub swap_total: i64, pub load1: f32, pub load5: f32, pub load15: f32, pub disk_used: i64, pub disk_total: i64, pub net_rx_bytes: i64, pub net_tx_bytes: i64, pub uptime_secs: i64 }`
- `insert_metrics(exec: impl PgExecutor, machine_id: Uuid, s: &MetricsSampleRow) -> Result<()>` — one INSERT, `ts` defaults to `now()`.
- `pub struct SparkRow { pub machine_id: Uuid, pub cpu_pct: Option<f32>, pub mem_pct: Option<f64> }`
- `recent_series_all(exec, per_machine: i64) -> Result<Vec<SparkRow>>` — the last `per_machine` samples for EVERY machine, ordered `(machine_id, ts ASC)`; `mem_pct = 100*mem_used/NULLIF(mem_total,0)`.
- `pub struct MetricPoint { pub ts: OffsetDateTime, pub cpu_pct: Option<f32>, pub mem_used: Option<i64>, pub mem_total: Option<i64>, pub swap_used: Option<i64>, pub swap_total: Option<i64>, pub load1: Option<f32>, pub disk_used: Option<i64>, pub disk_total: Option<i64>, pub net_rx_bytes: Option<i64>, pub net_tx_bytes: Option<i64> }`
- `metrics_history(exec, machine_id: Uuid, since: OffsetDateTime) -> Result<Vec<MetricPoint>>` — samples with `ts >= since`, ascending.
- `pub struct MachineDetail { pub id: Uuid, pub machine_id: String, pub hostname: String, pub os: Option<String>, pub kernel: Option<String>, pub arch: Option<String>, pub primary_ip: Option<String>, pub agent_version: Option<String>, pub status: String, pub last_seen_at: Option<OffsetDateTime>, pub enrolled_at: OffsetDateTime, pub tags: Vec<String>, pub notes: Option<String> }`
- `machine_detail(exec, id: Uuid) -> Result<Option<MachineDetail>>`.

- [ ] **Step 1: Write failing tests** (`#[sqlx::test]`, fresh DB per test) in `repo.rs`:
  - `insert_metrics_then_history_returns_it`: seed a `machines` row; `insert_metrics` twice; `metrics_history(id, now-1h)` returns 2 points ascending with the cpu_pct you inserted.
  - `recent_series_all_limits_per_machine`: seed 2 machines, insert 30 samples each; `recent_series_all(20)` returns 40 rows (20 per machine) grouped by machine.
  - `machine_detail_returns_row_or_none`: seed a machine; `machine_detail(id)` is `Some` with the hostname; a random uuid is `None`.

- [ ] **Step 2: Run to verify they fail.** `cargo test -p argus-server repo::tests::insert_metrics -p argus-server 2>&1 | tail` → FAIL (undefined).

- [ ] **Step 3: Implement.** Match columns in `crates/server/migrations/0002_metrics.sql`. `insert_metrics`:
```rust
sqlx::query!(
    "INSERT INTO metrics (machine_id, ts, cpu_pct, mem_used, mem_total, swap_used,
        swap_total, load1, load5, load15, disk_used, disk_total, net_rx_bytes,
        net_tx_bytes, uptime_secs)
     VALUES ($1, now(), $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    machine_id, s.cpu_pct, s.mem_used, s.mem_total, s.swap_used, s.swap_total,
    s.load1, s.load5, s.load15, s.disk_used, s.disk_total, s.net_rx_bytes,
    s.net_tx_bytes, s.uptime_secs,
).execute(exec).await?;
```
`recent_series_all` uses a window function:
```sql
SELECT machine_id, cpu_pct, (100.0 * mem_used / NULLIF(mem_total,0))::float8 AS "mem_pct?"
FROM (
  SELECT machine_id, ts, cpu_pct, mem_used, mem_total,
         row_number() OVER (PARTITION BY machine_id ORDER BY ts DESC) AS rn
  FROM metrics
) t
WHERE rn <= $1
ORDER BY machine_id, ts ASC
```
(bind `per_machine`; annotate nullability with `?` where sqlx can't infer). `metrics_history`: `SELECT ts, cpu_pct, mem_used, ... WHERE machine_id=$1 AND ts >= $2 ORDER BY ts ASC`. `machine_detail`: `SELECT id, machine_id, hostname, os, kernel, arch, host(primary_ip) as "primary_ip?", agent_version, status, last_seen_at, enrolled_at, tags, notes FROM machines WHERE id=$1` (`fetch_optional`).

- [ ] **Step 4: Run tests to pass.** `cargo test -p argus-server repo:: -- --ignored=false 2>&1 | tail` (the new ones are `#[sqlx::test]`, not `#[ignore]`) → PASS.

- [ ] **Step 5: Prepare + commit.**
```bash
cargo sqlx prepare --workspace -- --all-targets && git add .sqlx
cargo fmt --all
git add crates/server/src/repo.rs .sqlx
git commit -m "feat(server): metrics repo — insert + history/series/detail queries"
```

---

## Task 2: Server session handler — `MetricsSample` arm

**Files:** Modify `crates/server/src/grpc.rs`.

**Interfaces:** Consumes `repo::insert_metrics`, `repo::MetricsSampleRow`, `repo::touch_last_seen` (existing).

- [ ] **Step 1: Add a `metrics_row_from_proto` mapper + the match arm.** In `handle_agent_frame`, add:
```rust
Some(agent_frame::Payload::Metrics(m)) => {
    let row = metrics_row_from_proto(&m);
    repo::insert_metrics(pool, machine_id, &row).await?;
    repo::touch_last_seen(pool, machine_id).await?;
}
```
where `fn metrics_row_from_proto(m: &argus_proto::v1::MetricsSample) -> repo::MetricsSampleRow` casts each `u64` field `as i64` and each `float` to `f32`.

- [ ] **Step 2: Extend the seam test.** In `grpc.rs` tests, add `handle_agent_frame_metrics_inserts_row_and_touches_last_seen`: seed an enrolled machine; call `handle_agent_frame` with an `AgentFrame{ payload: Some(Payload::Metrics(MetricsSample{ cpu_pct: 12.5, mem_used: 100, mem_total: 200, ..Default::default() })) }`; assert a `metrics` row exists for the machine (`SELECT count(*)`), its `cpu_pct = 12.5`, and `last_seen_at` is set/fresh.

- [ ] **Step 3: Run.** `cargo test -p argus-server grpc::tests::handle_agent_frame_metrics 2>&1 | tail` → PASS.

- [ ] **Step 4: Prepare + commit.** `cargo sqlx prepare --workspace -- --all-targets`; `cargo fmt --all`; commit `grpc.rs` + `.sqlx` as `feat(server): persist MetricsSample from the Session stream`.

---

## Task 3: Server — hourly metrics prune

**Files:** Modify `crates/server/src/repo.rs`, `crates/server/src/jobs.rs`, `crates/server/src/main.rs`.

**Interfaces (Produces):** `repo::prune_metrics(exec, older_than: std::time::Duration) -> Result<u64>`; `jobs::prune_metrics(pool: PgPool) -> Result<()>`.

- [ ] **Step 1: `repo::prune_metrics` + test.** `DELETE FROM metrics WHERE ts < $1` binding `now - older_than` (compute the cutoff `OffsetDateTime::now_utc() - older_than` in Rust, like `mark_stale_offline`); return `rows_affected()`. `#[sqlx::test]` `prune_metrics_deletes_old_rows`: insert a row with `ts = now() - 3 days` (explicit INSERT with `ts`), one with `ts = now()`; `prune_metrics(48h)` returns 1 and leaves the fresh row.

- [ ] **Step 2: Run to fail, implement, run to pass.**

- [ ] **Step 3: `jobs::prune_metrics` loop.** Mirror `jobs::run` (the offline sweeper): `interval(Duration::from_secs(3600))`; each tick `repo::prune_metrics(&pool, Duration::from_secs(METRICS_RETENTION_HOURS as u64 * 3600))`, log the deleted count when > 0. Add `argus_common::METRICS_RETENTION_HOURS`. Wire into `main.rs`'s `try_join!` as a fourth arm (`jobs::prune_metrics(pool.clone())`).

- [ ] **Step 4: `cargo check` + fmt + prepare + commit** as `feat(server): hourly metrics retention prune`.

---

## Task 4: Server — metrics API surface

**Files:** Modify `crates/server/src/http.rs`.

**Interfaces:** Consumes `repo::{recent_series_all, machine_detail, metrics_history}`.

- [ ] **Step 1: Extend `/api/fleet`.** Add to `FleetRow`: `cpu_pct: Option<f32>`, `mem_pct: Option<f64>`, `spark_cpu: Vec<f32>`, `spark_mem: Vec<f64>`. In the `fleet` handler, after loading machines, call `repo::recent_series_all(&pool, 20)`, group rows by `machine_id` into `(Vec<cpu>, Vec<mem>)`, set each `FleetRow`'s `spark_*` and the latest (`cpu_pct`/`mem_pct` = last element or `None`). Keep ordering by hostname.

- [ ] **Step 2: `GET /api/machines/:id`.** `async fn machine(State(state), Path(id): Path<Uuid>) -> Result<Json<MachineDetailDto>, StatusCode>`: `repo::machine_detail` → `None` ⇒ `StatusCode::NOT_FOUND`; else map to a `#[derive(Serialize)] MachineDetailDto` (rfc3339 for `last_seen_at`/`enrolled_at`). Route `.route("/api/machines/{id}", get(machine))`.

- [ ] **Step 3: `GET /api/machines/:id/metrics`.** Parse `range` from the query (`#[derive(Deserialize)] struct RangeQuery { range: Option<String> }`); map `"1h"|"6h"|"24h"` → `Duration::hours(1|6|24)`, default `1h`, anything else ⇒ `StatusCode::BAD_REQUEST`. `repo::metrics_history(&pool, id, now - dur)` → `Json(Vec<MetricPointDto>)` (rfc3339 `ts`). Route `.route("/api/machines/{id}/metrics", get(machine_metrics))`.

- [ ] **Step 4: `oneshot` tests** (`#[sqlx::test]`): seed a machine + samples; assert `/api/fleet` body includes non-empty `spark_cpu`; `/api/machines/:id` returns 200 with the hostname and 404 for a random id; `/api/machines/:id/metrics?range=1h` returns an ascending array and `?range=bogus` returns 400.

- [ ] **Step 5: fmt + prepare + commit** as `feat(server): fleet sparkline data + machine detail + metrics history API`.

---

## Task 5: Agent — `sysinfo` sampler

**Files:** Create `crates/agent/src/metrics.rs`; modify `crates/agent/src/main.rs` (`mod metrics;`), `crates/agent/Cargo.toml`, root `Cargo.toml`.

**SPIKE FIRST:** the `sysinfo` API shifts across versions. Confirm the **0.39** API before coding (read the vendored `sysinfo-0.39*` source or docs.rs/sysinfo/0.39): `System::new`, `refresh_cpu_usage`, `global_cpu_usage`, `refresh_memory`, `used_memory`/`total_memory`/`used_swap`/`total_swap`, `System::load_average() -> LoadAvg{one,five,fifteen}`, `Networks::new_with_refreshed_list` + `NetworkData::total_received/total_transmitted`, `Disks::new_with_refreshed_list` + `Disk::total_space/available_space`, `System::uptime`. Record the confirmed calls as a comment.

**Interfaces (Produces):** `pub struct Sampler { sys: System, nets: Networks, disks: Disks }`; `Sampler::new() -> Sampler` (does an initial refresh so the first CPU delta is valid); `Sampler::sample(&mut self, agent_version: &str) -> argus_proto::v1::MetricsSample`.

- [ ] **Step 1: Add the dep.** Root `Cargo.toml` `[workspace.dependencies]`: `sysinfo = "0.39"`. Agent `Cargo.toml`: `sysinfo.workspace = true`. Add `# sysinfo -> slice 2 (metrics)` is already noted there.

- [ ] **Step 2: Write a failing test.** In `metrics.rs`: `sample_populates_core_fields`: `let mut s = Sampler::new(); std::thread::sleep(std::time::Duration::from_millis(300)); let m = s.sample("test");` assert `m.mem_total > 0`, `m.uptime_secs > 0`, `m.agent_version == "test"`, and `m.cpu_pct >= 0.0` (CPU may be 0 on an idle host — assert non-negative and `<= 100.0 * num_cpus` loosely, i.e. `>= 0.0`).

- [ ] **Step 3: Run to fail; implement `Sampler`.** Per the spike: refresh cpu+memory on `sys`, refresh `nets`/`disks`; fill `MetricsSample` (`cpu_pct = global_cpu_usage()`, mem/swap from `sys`, `load*` from `load_average()` as `f32`, disk = Σ(total−available)/Σtotal, net = Σ total_received/transmitted, `uptime_secs`, `unix_ms` from wall clock, `extra_json: String::new()`). Cast `u64→` proto `u64` directly (proto fields are `u64`).

- [ ] **Step 4: Run to pass; fmt; commit** as `feat(agent): sysinfo host-metrics sampler`.

---

## Task 6: Agent — send `MetricsSample` on the session

**Files:** Modify `crates/agent/src/session.rs`.

**Interfaces:** Consumes `metrics::Sampler` (Task 5).

- [ ] **Step 1: Wire the sender.** In `connect_and_serve`'s heartbeat sender task, construct `let mut sampler = metrics::Sampler::new();` before the loop. On each `ticker.tick()`, instead of (or in addition to) the `Heartbeat`, send `AgentFrame{ stream_id: CONTROL_STREAM_ID, payload: Some(agent_frame::Payload::Metrics(sampler.sample(env!("CARGO_PKG_VERSION")))) }`. Keep sending a `Heartbeat` too (cheap, and covers the pre-first-sample window) OR rely on metrics for liveness — send BOTH each tick for simplicity. The `Hello` snapshot on connect is unchanged.

- [ ] **Step 2: `cargo check -p argus-agent`** compiles; `cargo build -p argus-agent 2>&1 | grep -c '^warning'` → 0. (The real behavior is covered by the Task 10 E2E; the sampler itself is unit-tested in Task 5.)

- [ ] **Step 3: fmt; commit** as `feat(agent): stream MetricsSample every 15s over the Session`.

---

## Task 7: Frontend — routing + api client

**Files:** Modify `frontend/package.json`, `frontend/src/main.tsx`, `frontend/src/App.tsx`, `frontend/src/api.ts`.

- [ ] **Step 1: Add the dep.** `npm --prefix frontend install react-router-dom` (adds to package.json + lock).

- [ ] **Step 2: Router.** `main.tsx`: wrap `<App/>` in `<BrowserRouter>`. `App.tsx`:
```tsx
import { Routes, Route } from "react-router-dom";
import FleetPage from "./FleetPage";
import MachineDetailPage from "./MachineDetailPage";
export default function App() {
  return (
    <Routes>
      <Route path="/" element={<FleetPage />} />
      <Route path="/machines/:id" element={<MachineDetailPage />} />
    </Routes>
  );
}
```

- [ ] **Step 3: `api.ts`.** Extend `FleetRow` with `cpu_pct: number|null; mem_pct: number|null; spark_cpu: number[]; spark_mem: number[]`. Add:
```ts
export type MachineDetail = { id: string; hostname: string; os: string|null; kernel: string|null; arch: string|null; primary_ip: string|null; agent_version: string|null; status: string; last_seen_at: string|null; enrolled_at: string; tags: string[]; notes: string|null; };
export type MetricPoint = { ts: string; cpu_pct: number|null; mem_used: number|null; mem_total: number|null; swap_used: number|null; swap_total: number|null; load1: number|null; disk_used: number|null; disk_total: number|null; net_rx_bytes: number|null; net_tx_bytes: number|null; };
export async function getMachine(id: string): Promise<MachineDetail> { const r = await fetch(`/api/machines/${id}`); if (!r.ok) throw new Error(`machine ${r.status}`); return r.json(); }
export async function getMetrics(id: string, range: "1h"|"6h"|"24h"): Promise<MetricPoint[]> { const r = await fetch(`/api/machines/${id}/metrics?range=${range}`); if (!r.ok) throw new Error(`metrics ${r.status}`); return r.json(); }
```
> Since MachineDetailPage doesn't exist yet, add a one-line placeholder `export default function MachineDetailPage(){ return null; }` in `MachineDetailPage.tsx` so this task typechecks; Task 9 fills it in.

- [ ] **Step 4: Verify.** `npm --prefix frontend run typecheck` and `npm --prefix frontend run build` clean.

- [ ] **Step 5: Commit** (source only — not `dist/`/`node_modules`) as `feat(web): react-router + metrics API client`.

---

## Task 8: Frontend — fleet grid sparklines

**Files:** Create `frontend/src/Sparkline.tsx`; modify `frontend/src/FleetPage.tsx`.

- [ ] **Step 1: `Sparkline` component (inline SVG, no lib).** `function Sparkline({ values, max = 100 }: { values: number[]; max?: number })` → renders an `<svg viewBox="0 0 100 24">` `<polyline>` mapping `values` to points (`x = i/(n-1)*100`, `y = 24 - (v/max)*24`); render nothing for `<2` points. Small, presentational.

- [ ] **Step 2: FleetPage.** For each row, add cells: current cpu% (`row.cpu_pct?.toFixed(0)`), current mem% , `<Sparkline values={row.spark_cpu} />`, `<Sparkline values={row.spark_mem} />`. Make the hostname a `<Link to={`/machines/${row.id}`}>`. Keep the existing 5s poll, status badge, and reconnecting logic.

- [ ] **Step 3: Verify** `typecheck` + `build` clean. Commit as `feat(web): fleet grid cpu/mem sparklines + machine links`.

---

## Task 9: Frontend — machine detail page

**Files:** Replace the placeholder `frontend/src/MachineDetailPage.tsx`.

- [ ] **Step 1: Build the page.** `useParams` for `id`; `useState` for `range` (default `"1h"`); poll `getMachine(id)` + `getMetrics(id, range)` every 10s (and on `range` change). Render: a header (hostname, os, ip, status badge, tags, last-seen), a range selector (1h/6h/24h buttons), and charts for **cpu%**, **mem%** (`100*mem_used/mem_total`), **load1**, and **net rate** (`Δnet_rx_bytes / Δseconds` between consecutive points, same for tx). Handle loading/error/empty and an unknown-id (404 → "machine not found") state. A "← Fleet" link back to `/`.

- [ ] **Step 2: Charts.** Confirm `@e412/rnui-react`'s chart export from `frontend/node_modules/@e412/rnui-react/dist/index.d.ts` (grep `Chart`/`Line`/`Area`) and use it; if the API is unclear, reuse the `Sparkline` SVG approach scaled up (a larger line chart) rather than guessing. A clean production build is the hard requirement.

- [ ] **Step 3: Verify** `typecheck` + `build` clean. Commit as `feat(web): machine detail page with metric charts`.

---

## Task 10: End-to-end verification

- [ ] **Step 1: Rebuild + run.** Frontend build, `cargo build --workspace`. Start the control plane and a real agent as in `docs/DEV.md` (dev Postgres is up; reuse the enrolled agent identity or re-enroll with a fresh token).

- [ ] **Step 2: Verify metrics flow.** Within ~1 min: `psql` shows rows accumulating in `metrics` for the machine; `curl /api/fleet` shows non-empty `spark_cpu`/`spark_mem` and a `cpu_pct`; the fleet page renders sparklines; opening `/machines/:id` shows charts that update.

- [ ] **Step 3: Verify prune.** Insert a synthetic old row (`ts = now() - 3 days`) via `psql`, confirm the hourly prune (or call `repo::prune_metrics` via a short test) removes it while keeping fresh rows.

- [ ] **Step 4: Record results in `docs/DEV.md`; commit.**

---

## Self-review notes (author)
- **Spec coverage:** sampler (T5) → send (T6) → persist+liveness (T2) → prune (T3) → fleet series + detail + history API (T1/T4) → routing + grid sparklines + detail charts (T7/T8/T9) → E2E (T10). All spec sections covered.
- **Type consistency:** `MetricsSampleRow`/`SparkRow`/`MetricPoint`/`MachineDetail` defined in T1 and consumed by name in T2/T4; proto `MetricsSample` (u64/float) → row (`i64`/`f32`) cast is stated in both T2 and Global Constraints.
- **Two spikes-lite:** sysinfo 0.39 API (T5) and rnui chart export (T9) are confirm-before-code, each with a concrete fallback (the second falls back to the T8 SVG approach).
- `ts = now()` server-side (T1) is deliberate; `unix_ms` in the proto is informational and unused server-side.
