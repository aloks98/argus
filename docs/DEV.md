# Local development

How to run the Spine slice (enroll → mTLS → heartbeat → fleet page) end-to-end on
one machine.

## Prerequisites
- A dev Postgres. Quickest:
  ```bash
  docker run -d --name argus-pg -e POSTGRES_PASSWORD=argus -e POSTGRES_DB=argus \
    -p 5432:5432 postgres:17
  ```
- `DATABASE_URL` must be set for `sqlx`'s compile-time-checked queries (a committed
  `.sqlx` offline cache also lets `SQLX_OFFLINE=true` builds work without a DB):
  ```bash
  export DATABASE_URL='postgres://postgres:argus@localhost:5432/argus'
  ```
- Migrations run automatically on control-plane startup (`sqlx::migrate!`); to apply
  them manually: `sqlx migrate run --source crates/server/migrations`.

## Build
```bash
npm --prefix frontend ci && npm --prefix frontend run build   # dist/ embedded by the server
cargo build --workspace
```

## Run the control plane
```bash
export ARGUS_DATABASE_URL="$DATABASE_URL"
export ARGUS_FIELD_KEY="$(head -c 32 /dev/urandom | base64)"   # keep stable across restarts
export ARGUS_HTTP_ADDR=127.0.0.1:8080
export ARGUS_AGENT_ADDR=127.0.0.1:9443
cargo run -p argus-server
```
On first boot it generates + persists the internal CA (`ca_material`, key encrypted
with `ARGUS_FIELD_KEY`); on later boots it *loads* it. The browser surface is on
`:8080`, the agent mTLS gRPC surface on `:9443`.

> Reuse the same `ARGUS_FIELD_KEY` across restarts — it decrypts the stored CA key.
> A `.cargo/config.toml` `[env]` block is a convenient place to pin the dev values so
> every `cargo run`/`test` sees them (git-ignored).

## Enroll an agent
```bash
# 1. Grab the CA cert the agent needs to verify the server:
docker exec argus-pg psql -U postgres -d argus -tAc \
  "SELECT cert_pem FROM ca_material WHERE id=1" > /tmp/argus-ca.crt

# 2. Create an enrollment token (raw token 'devtoken'; only its sha256 is stored):
HASH=$(printf devtoken | sha256sum | cut -d' ' -f1)
docker exec argus-pg psql -U postgres -d argus -c \
  "INSERT INTO enrollment_tokens (name, token_hash) VALUES ('dev', decode('$HASH','hex'))"

# 3. Run the agent (ARGUS_DATA_DIR lets it write its key/cert as a non-root user):
ARGUS_AGENT_ENDPOINT=https://localhost:9443 \
ARGUS_JOIN_TOKEN=devtoken \
ARGUS_CA_CERT=/tmp/argus-ca.crt \
ARGUS_DATA_DIR=/tmp/argus-agent \
cargo run -p argus-agent
```
The agent generates a keypair + CSR locally (private key never leaves the host),
calls `Enroll` over server-auth TLS, persists the issued client cert to
`ARGUS_DATA_DIR`, then opens the persistent mTLS `Session` and heartbeats. On a
later start it *loads* that identity and skips enrollment.

Open http://127.0.0.1:8080 — the fleet page shows the machine `online`.

## Spine end-to-end verification (2026-07-06)
Verified manually per the plan's Task 11 (`docs/plans/2026-07-05-spine-slice.md`):

- Agent enrolled (`agent_id=284b631f-…`) and connected over mTLS; `GET /api/fleet`
  returned it `online` (`hostname=fatman`, `os=Debian 13 (trixie)`,
  `primary_ip=192.168.150.40`). `audit_log`: `agent.enroll=ok`, `agent.online=ok`.
- **CA persistence:** restarting the control plane logged *"loaded existing CA from
  ca_material"* (no regeneration) — the stateless-pod reschedule property.
- **Load-first:** restarting the agent reused its on-disk identity — no second
  `agent.enroll` row.
- **Self-heal:** killing the control plane mid-session, the agent logged
  `session failed; backing off` (exponential backoff + jitter, backoff correctly
  *not* reset for a sub-30s session); on restart it reconnected on its own and the
  machine returned to `online` with a fresh `last_seen_at` — no agent restart.

## Metrics slice end-to-end verification (2026-07-07)
Verified manually per the plan's Task 10 (`docs/plans/2026-07-07-metrics-slice.md`):

- A live agent enrolled, connected, and streamed `MetricsSample` frames every 15s;
  rows accumulated in `metrics`.
- `GET /api/fleet` returned per-machine `cpu_pct` + a `spark_cpu`/`spark_mem` series
  (e.g. `spark_cpu:[2.32,1.17,0.96]`); the fleet grid renders inline-SVG sparklines.
- `GET /api/machines/:id` returned machine detail; `GET /api/machines/:id/metrics?range=1h`
  returned the ascending time series; `?range=bogus` returned HTTP 400.
- Retention: a 3-day-old row was removed by `DELETE FROM metrics WHERE ts < now() - 48h`
  (the hourly `jobs::prune_metrics` task) while fresh rows remained.

## Docker slice end-to-end verification (2026-07-23)
Verified manually against the live Docker daemon on the dev host (Task 5 of the
Docker slice). A throwaway `argus-verify-test` container (from the cached
`registry:3` image, no published ports) was the only verb target; every other host
container was read-only.

- **State:** with the agent connected (`machine_id=56c5ab05-…`, `hostname=fatman`,
  `online`), `GET /api/machines/:id/docker` returned the host's full container list
  with `state`/`status`/`health` populated — `argus-verify-test` as
  `state:"running"` alongside the real host containers the agent also reports
  (`argus-pg`, `registry-cache` shown `health:"healthy"`, `buildx_buildkit_*`,
  plus `exited` ones like `eye-victoriametrics-1`).
- **Verbs (only against `argus-verify-test`):** each POST returned HTTP 200 with
  `{"ok":true,"message":"ok","status":"completed"}` and flipped the *real*
  container state on the next `docker ps`:
  - `…/stop`  → container `Exited (2)`.
  - `…/start` → back to `Up`.
  - `…/restart` → `Up` (fresh uptime).
- **Audit trail:** `audit_log` gained `container.stop` / `container.start` /
  `container.restart` rows, each `result = ok`, `actor = anonymous`, `target_ref`
  = the container id.
- **Offline path:** killing the agent process, the server logged
  `session: agent disconnected`; a subsequent `…/restart` POST returned **HTTP 409**
  (`agent not connected`, from `DispatchError::NotConnected`) and wrote a
  `container.restart` row with `result = denied` — the verb was never dispatched to
  a daemon.
