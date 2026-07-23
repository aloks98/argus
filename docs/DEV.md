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

## Frontend design system (2026-07-23)

The SPA was given a deliberate visual identity and a shared component/data layer,
before the systemd / logs / terminal slices each add another screen. Plan:
`docs/plans/2026-07-23-frontend-design-system.md`; design of record:
`docs/superpowers/specs/2026-07-23-frontend-design-system-design.md`.

**Direction — "asset tag".** Industrial rack labelling: a hostname (or container
name) renders as a solid stencilled tag whose *fill is its status* — that tag is
the signature element. Hazard yellow `#FFE600` on true black, square corners
(`--radius: 0`), 2px rules, Archivo / Archivo Black / IBM Plex Mono (mono = machine
truth: hostnames, IPs, container ids, `Exited (2)`).

**Shell.** A left sidebar built on **rnui's `Sidebar` suite** (`SidebarProvider` /
`Sidebar` / `SidebarMenuButton` / `SidebarInset`) carries nav, the fleet summary
and the theme toggle; content is capped at `max-w-6xl` and centred. Collapsing,
the mobile sheet and the Cmd/Ctrl+B shortcut come from the library rather than
being hand-rolled, and rnui's own `--sidebar*` token set is overridden in both
themes so it renders in the Argus palette. The sidebar renders from the shared
route config in `src/app/routes.tsx`, which `App.tsx` also renders `<Routes>`
from — so a new page becomes a route *and* a nav entry in one edit. Routes are
real paths: `/machines` lists, `/machines/:id` details, `/` redirects.

**Prefer the library component.** Sidebar, nav-link active state, status tags and
breadcrumbs were each hand-rolled before being replaced by rnui's `Sidebar`,
React Router's `NavLink`, rnui's `Badge` and rnui's `Breadcrumb`. Check what
`@e412/rnui-react` already exports before building chrome — its `index.d.ts` is
the fastest way to look.

**Machine page.** A breadcrumb, the hostname as the page title, and a `SpecStrip`
— facts as labelled cells rather than a run-on meta line, so fields can be added
or omitted without the reader parsing by position. `SpecStrip` is generic
(`{label, value}[]`) and is the intended home for systemd/log page facts too.

**Light mode inverts brand chrome.** `#FFE600` is right on black and glaring on
white, so in light mode brand surfaces become an ink block with yellow text
(`--primary: #14161A`, `--primary-foreground: #FFE600`). Chart series colours are
likewise darkened in light mode via `--chart-*`. Status *fills* stay identical
across themes — that is what keeps the asset tag ground-agnostic.

**Where the theme lives.** `frontend/src/index.css` overrides rnui's shadcn-style
CSS variables, so every rnui component inherits the identity. Two cascade traps
worth knowing if you edit it:

- `--font-sans` **must** be declared in the plain unlayered `:root` block. A
  Tailwind `@theme {}` block compiles into `@layer theme`, and rnui-themes
  declares its own unlayered `--font-sans` — an unlayered rule always beats a
  layered one, so an `@theme`-only value silently never applies.
- Status fills must be identical in both themes (that is what makes the tag
  ground-agnostic). `--idle` is its own token for exactly this reason:
  `--muted-foreground` differs per theme and could not pass contrast in both.

**Overriding an rnui class: match its modifier exactly.** This has bitten twice,
and neither `tsc` nor `npm run build` can see it — the CSS compiles, it just
never wins:

- rnui styles its sidebar button's selected state with Tailwind's *presence*
  form, `data-active:` (Base UI emits `data-active=""`, not `="true"`). An
  override written as `data-[active=true]:…` compiles to a rule that can never
  match.
- rnui's sidebar container carries `group-data-[side=left]:border-r`, specificity
  (0,2,0). A plain `border-r-2` is (0,1,0), and `tailwind-merge` will not dedupe
  them because the modifiers differ — so both survive and rnui's 1px wins.

The mechanical rule: **write your override with the same modifier the base uses**
(`group-data-[side=left]:border-r-2`, `data-active:bg-primary/15`). Then
`tailwind-merge` collapses the pair and yours wins deterministically, instead of
the outcome depending on stylesheet emission order. When an rnui override looks
correct but has no effect, check its modifier before anything else.

**Contrast (WCAG AA 4.5:1), computed rather than eyeballed.** Tag text is black on
every fill: ok `#00E676` 12.58:1 · warn `#FF6D00` 7.44:1 · fail `#FF1744` 5.46:1 ·
idle `#8A8A8A` 6.08:1. Note white on `fail` would be only **3.85:1** — saturated
reds read better with black text, which is the opposite of the usual instinct.

**Libraries adopted:** TanStack Query owns all server state (polling is
`refetchInterval`; container verbs are `useMutation` + `invalidateQueries`), `cva`
types the status variants, and uPlot renders every chart including the fleet
sparklines. Deliberately *not* added: a date library (`Intl.RelativeTimeFormat`),
TanStack Table, zod.

**Bundle: 1,389 kB → 678.88 kB (−51%; gzip 223.53 kB).**
echarts + zrender were ~55% of the bundle. Removing the `LineChart` import saved
only **0.68 kB** — `@e412/rnui-react` ships a single barrel that runs echarts'
side-effectful `use([...])` registration at module scope, so Rollup cannot
tree-shake it no matter what you import. `frontend/src/stubs/echarts.ts` plus a
`vite.config.ts` alias replaces it with no-ops.

> **Removal condition for the stub:** delete `src/stubs/echarts.ts` and the
> `resolve.alias` entry in `vite.config.ts` once `@e412/rnui-react` declares
> `"sideEffects": false` and moves the echarts registration inside its chart
> component. Until then the stub must export every echarts symbol the barrel
> imports; a future rnui version importing a new one fails the build with a clear
> `"X is not exported by src/stubs/echarts.ts"` — that error is the intended
> tripwire, not a breakage.

The remaining ~679 kB is legitimate app + vendor (React, rnui/base-ui), not bloat.
Sub-500 kB was not reachable once echarts was gone, so `build.chunkSizeWarningLimit`
is set to **750** — above the measured floor, low enough that genuine new bloat
still trips it.

**Verified:** `npx tsc --noEmit` and `npm run build` clean (note `npm run build` is
`vite build` only and does **not** type-check — run `tsc` separately);
`zrender` absent from the built output; zero hard-coded grays or `max-w-5xl`
wrappers remain; contrast ratios computed for every fill; status is never conveyed
by colour alone (fleet `STATUS` column, container state as text, and the status
word leading the machine page's meta line — the hostname tag's fill encodes status
by colour, so a text carrier has to survive on that page).

**Not yet verified:** fleet-page density at a realistic ~40-guest fleet (≈80 uPlot
sparkline instances) — the dev fleet has one machine, so there is no evidence
either way; measure it when the fleet grows.

## Systemd slice end-to-end verification (2026-07-23)

Verified manually against the live system bus on the dev host (Task 12 of the
systemd slice). The agent was run **as root** so polkit permits unit verbs — as
an unprivileged user the bus returns `InteractiveAuthorizationRequired`, which is
also why the `#[ignore]`d live-bus tests in `crates/agent/src/systemd.rs` need
`sudo`. Two throwaway units were the *only* verb targets; no real host unit was
started, stopped, or restarted at any point.

```bash
# The two disposable targets (removed again afterwards):
sudo tee /etc/systemd/system/argus-verify-test.service <<'UNIT'
[Unit]
Description=Argus systemd slice verification target
[Service]
Type=simple
ExecStart=/bin/sleep infinity
UNIT
sudo tee /etc/systemd/system/argus-verify-fail.service <<'UNIT'
[Unit]
Description=Argus systemd slice failure-path target
[Service]
Type=oneshot
ExecStart=/bin/false
UNIT
sudo systemctl daemon-reload
```

- **State:** with the agent connected (`machine_id=56c5ab05-…`, `hostname=fatman`,
  `online`), `GET /api/machines/:id/systemd` returned **127 units**, every one a
  `*.service`, none `failed` on a healthy host. The failed-unit query contributes
  nothing extra until something actually fails, which is the intended shape.
- **Verbs (only against `argus-verify-test.service`):** each POST returned HTTP 200
  with `{"ok":true,"message":"done","status":"completed"}` and flipped the *real*
  unit state:
  - `…/start`   → `systemctl is-active` = `active`.
  - `…/restart` → `active` (fresh).
  - `…/stop`    → `inactive`.
- **The failure path — the assertion this slice exists for.** `argus-verify-fail.service`
  has `ExecStart=/bin/false`, so systemd *accepts* the start job and the unit then
  fails. `…/start` returned `{"ok":false,"message":"systemd job result: failed"}`.
  A naive enqueue-and-return implementation would have reported `ok:true` here.
  This is the check to re-run if the `JobRemoved` correlation is ever touched.
- **Self-preservation guard.** Verified in two halves, because the obvious direct
  test is destructive:
  - *The refusal path fires.* Before the `GetUnitByPID` fix below — when
    `self_unit` was the hard-coded `argus-agent.service` fallback — both
    `…/units/argus-agent.service/stop` and the bare `…/units/argus-agent/stop`
    returned
    `{"ok":false,"message":"refusing to operate on the unit hosting this agent"}`,
    and the agent stayed `online`. That exercises the live guard code path
    including bare-name normalisation.
  - *It consumes the discovered value, not the fallback.* After the fix,
    `self_unit` resolves to `session-9.scope` and that same
    `…/units/argus-agent.service/stop` is **no longer** refused — it reaches
    systemd and returns `NoSuchUnit`.
  - **Not tested:** a `stop` against the real discovered unit while the agent is
    running under it. On this host that unit is the shell session scope, so the
    test would kill the agent and the session — and if the guard were broken,
    that is precisely what would happen. The positive case rests on the two
    observations above plus the `is_self_unit` unit tests (exact normalised
    equality, including the `argus-agent-proxy.service` lookalike). Re-check it
    on a host where the agent runs under a disposable unit if you want the
    direct proof.
- **Validation:** a `%2F`-encoded unit name → HTTP 400 (`invalid unit name`); an
  unknown action → HTTP 400.
- **Audit trail:** `audit_log` gained six `unit.*` rows — `unit.start` /
  `unit.restart` / `unit.stop` with `result = ok`, and `result = error` for the
  failing unit and both refused self-guard attempts — each `actor = anonymous`
  with `target_ref` = the unit name.
- **Fleet rollup:** once `argus-verify-fail.service` was left in `failed`, the next
  15s snapshot moved `/api/fleet`'s `failed_units` from `0` to `1`, and
  `/api/machines/:id/systemd` listed the unit as `failed/failed`.
- **Offline path:** with the agent stopped, a `…/restart` POST returned **HTTP 409**
  (`agent not connected`) and wrote a `unit.restart` row with `result = denied` —
  the verb was never dispatched.

### One bug this pass caught that nothing static could

`#[proxy] fn get_unit_by_pid` makes zbus derive the D-Bus method name
`GetUnitByPid`, but systemd's method is **`GetUnitByPID`**. Every call failed with
`UnknownMethod`, so `discover_self_unit` fell back to the compiled-in
`argus-agent.service` on *every* host — silently reinstating exactly the
hard-coded assumption the runtime discovery was added to remove. It looked like it
worked, because the guard still refused *something*.

It was invisible to `cargo test`, `clippy`, and the type system: the unit tests
inject `self_unit` directly, and the fallback made failure indistinguishable from
success apart from one `warn!` line. Fixed with `#[zbus(name = "GetUnitByPID")]`.
After the fix `self_unit` resolves to the true hosting unit (`session-9.scope`
when the agent is run from a shell), and `argus-agent.service` — previously
guard-refused via the fallback — reaches systemd and returns `NoSuchUnit`.

**Lesson worth keeping:** zbus's snake_case → CamelCase derivation is wrong for any
D-Bus method with non-standard capitalisation. Every *other* proxy method here was
exercised live (unit listing, all three verbs, `JobRemoved`), so `GetUnitByPID` was
the only name that could hide.

**Not yet verified:** the Units tab at a realistic unit count in a browser — 127
rows is well past the 40–90 the design assumed, so the filter and failed-first sort
carry more weight than expected; a human visual pass should confirm the table stays
readable and the "failed only" checkbox toggles.
