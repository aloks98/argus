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

## Running the `#[ignore]`d live agent tests

The agent's `live_*` tests talk to the real journal and D-Bus system bus, so they
are `#[ignore]`d by default. They need **root**: `journalctl` returns nothing
unless you're in `systemd-journal`/`adm`, and polkit denies the unit-verb test
with `InteractiveAuthorizationRequired`. Build unprivileged, then run the test
binary as root — `sudo cargo test` would use a different `CARGO_HOME`/target and
rebuild the world:

```bash
cargo test -p argus-agent --no-run          # prints the test binary path
sudo -n ./target/debug/deps/argus_agent-<hash> --ignored --test-threads=1
```

Do **not** "fix" a failure here by adding your user to `systemd-journal` or
editing sudoers — the tests are ignored precisely because they need privilege the
normal test run shouldn't have.

**CI does not run these.** The act_runner is a container with no D-Bus system
bus, no journal, and no root-for-polkit, so `.forgejo/workflows/ci.yml` runs
`cargo test --workspace -- --ignored --skip live_`. Every other ignored test
(the server's, which only need the Postgres the job provisions) still runs
workspace-wide; only the host-dependent `live_*` set is excluded, and it is a
manual real-host gate instead.

> Name any test that needs a real systemd host `live_*`. That prefix is what CI
> filters on — a host-dependent test called anything else will run in the
> container and fail.

## Enroll an agent
The `/enroll` page in the UI is the normal way to mint a token; the `psql` steps
below are the no-UI fallback.
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

## Log tailing slice end-to-end verification (2026-07-23)

Verified live against the real system bus and Docker daemon on the dev host.
Design of record: `docs/superpowers/specs/2026-07-23-log-tailing-design.md`. The
agent runs **as root** — journal access for arbitrary units and the Docker
socket both need it, the same reason the systemd slice's live tests do.

- **Journal snapshot** (`?source=journal:ssh.service&tail=5&follow=false`):
  returned NDJSON `data:` events — `{"ts","level","ident","msg"}` — followed by
  an empty `eof` event, and the stream terminated (follow=false is a snapshot).
- **Docker logs** (`?source=docker:<name>`): streamed the container's output with
  `level:null` — Docker has no syslog severity, so the viewer renders those
  lines without a severity colour, as designed.
- **The lifecycle claim — a tail dies with its viewer.** With a `follow=true`
  tail open, exactly one `journalctl -u ssh.service` process was running; killing
  the client left **zero** within 3s. The chain — SSE stream drops →
  `TailGuard::drop` sends `LogTailStop` + `close_tail` → agent aborts the tailer
  → `kill_on_drop` SIGKILLs `journalctl` — works end to end. This is the check to
  re-run if the disconnect handling is ever touched:
  ```bash
  curl -sN ".../logs/stream?source=journal:ssh.service&follow=true" & sleep 4; kill %1
  sleep 3; pgrep -af 'journalctl.*ssh.service' || echo "no orphan — correct"
  ```
- **Backpressure — logs are best-effort, heartbeats are not.** A disposable unit
  running `while true; do echo …; done` was tailed two ways:
  - *Fast consumer:* ~34,700 lines in 12s, and the machine stayed `online` at
    every poll with the heartbeat sender never exiting.
  - *Slow consumer* (reading ~200 B every 0.3 s): the pipeline saturated, the
    agent dropped batches, and the `—— N lines dropped (stream saturated) ——`
    marker reached the client — while the machine **still** stayed `online`.

    This is the design's whole justification: a flooding unit degrades to a
    visible gap rather than starving the heartbeat into the 45s offline sweeper.
    If a busy log ever flaps a machine offline, the agent's `try_send` path is
    wrong.
- **Validation** (server-side, before any tail opens): `%20` (space), a
  `%2F`-encoded path traversal, a `%2F` in a docker ref, and an unknown scheme
  each returned **400**. **Offline agent** returned **409**.
- **Audit:** every open wrote a `logs.open` row with the source as `target_ref`
  and result `ok`.
- **Idle flush** (the one bug live testing caught in review): the `#[ignore]`d
  `live_idle_tail_…` agent test tails a quiet unit and asserts its backlog
  arrives without waiting for a new line. It passes under `sudo` in ~0.1s and
  **times out at 5s if the flush ticker is reverted** — a genuine regression
  guard. CI cannot run it (needs root), so it is part of this manual pass.

The disposable `argus-flood.service` was removed and the host left with no
`argus-*` units and no failed units.

## Log pagination — manual verification (2026-07-24)

Journal-only "load older" pagination on top of the live SSE tail. Design of
record: `docs/superpowers/specs/2026-07-24-log-pagination-design.md`. Agent runs
as root (journal access), same as the log slice.

**Endpoint checks (curl), against the live journal:**
- Grab an oldest cursor from a tail (`…/logs/stream?…&follow=false`), then
  `GET …/logs/page?source=journal:<unit>&before=<cursor>&limit=N` returns up to N
  older lines, each carrying a `cursor`, plus `oldest_cursor` and `reached_start`.
- **Exact chaining, no duplicate boundary:** paging before page 1's
  `oldest_cursor` returned page 2 whose lines did **not** include page 1's oldest
  cursor — the agent drops the inclusive anchor entry (`finalize_page`), so the
  seam never repeats a line.
- **`reached_start`:** paging a low-volume unit (`systemd-journald.service`) with
  `limit=1000` returned 411 lines (< limit) and `reached_start:true`.
- **Validation:** `source=docker:…` → **400** (docker has no cursor, unsupported);
  missing `before` → **400**; agent offline → **409**.
- **Audit:** a *successful* page fetch writes a `logs.page`/`ok` row. The row is
  written only after the tail is dispatched (mirroring `logs.open`), so a **409**
  (offline) or **504** (wedged) leaves **no** row for a read that never happened.
- **Agent hygiene:** the endpoint sends a `LogTailStop` once the page is
  collected, so a page read doesn't leak a dead `AbortHandle` on the agent; a
  non-zero `journalctl` exit (e.g. a rejected cursor) surfaces as a marker line
  rather than a silent empty page reported as `reached_start`.

**Browser checks (the behaviours static gates and curl cannot confirm — curl
does not do EventSource framing or scroll):** open a unit's logs, then:
1. Live tail streams and follows at the bottom (unchanged).
2. Scroll up → "loading older…" → older lines prepend and **the viewport holds**
   (the line you were reading stays put). Crucially, keep scrolling up so **two+
   consecutive pages** load — the trigger must re-fire on every load. Two library
   traps made this fail: a fixed-500 page size made the naïve `scrollToLine` skip
   re-anchoring on the 2nd+ load (fixed by clearing the anchor to `undefined`
   before each fetch); and `LazyLog`'s `onScroll` only fires on an offset *change*
   while its `align:"nearest"` anchor often left the viewport pinned at offset 0,
   so once parked at the top no scroll event could re-fire the load. Fixed by
   deferring the anchor two frames (so it lands off the top) plus an `onWheel`
   handler that fires the load on an upward wheel at the top even when no scroll
   event is produced.
3. At the journal start, "— beginning of journal —" shows and no further fetches
   fire.
4. Follow **pauses** while scrolled up (new live lines don't yank you down);
   scrolling back to the bottom **resumes** it.
5. Text stays selectable/copyable; a **docker** source shows no "load older" row.

## Full system journal + log filters — manual verification (2026-07-24)

Whole-journal source (`journal:@system`) plus a priority ceiling and a time
window, applied to per-unit journal reads too. Design of record:
`docs/superpowers/specs/2026-07-24-full-journal-design.md`. Agent runs as root
(journal access), same as the earlier log slices.

**The constraint that shapes the whole design** (verified against the live
journal, and the reason the window is not simply an argv flag):

| Combination | Result |
|---|---|
| `-p <n>` + `--cursor` | composes |
| `-b` + `--cursor` | composes; paging past the boot start returns only the inclusive anchor |
| `--since` + `--cursor` | **rejected** — "Please specify only one of --since=, --cursor=, --cursor-file=, and --after-cursor=" |
| `--since @<epoch>` (no cursor) | works |

So a relative window rides the live tail as `--since`, but a cursor-anchored page
read applies it as a **timestamp cutoff in `finalize_page`** instead. The cutoff
is a `take_while` over the descending records, not a `filter`, so the drop is
structurally a *suffix* — that is what makes the server's existing
`reached_start = lines.len() < limit` rule still mean "start of the window" even
if the journal contains a non-monotonic timestamp.

**Endpoint checks (curl), against the live journal:**
- `journal:@system` streams end-to-end — 8 records, each carrying a cursor.
- **Priority:** `priority=4` returned only levels `{3,4}`; zero entries above the
  ceiling. (Lower is more severe: `-p 4` returns 4,3,2,1,0.)
- **Window, as an A/B on the same cursor and limit:** `window=1h` returned 20
  lines, all within the hour, `reached_start:true`; `window=all` returned a full
  200 with `reached_start:false` and 180 of them older than an hour. This is the
  proof the cutoff works *and* that `reached_start` means "start of the window".
- **Validation:** `priority=9`, `priority=abc`, `window=nonsense` and a `docker:`
  source each returned **400**.
- **Audit:** `audit_log.target_ref` records the filters —
  `journal:@system p<=4 boot`, `journal:@system since=<ms>`, and a bare source
  when unfiltered. A docker read never carries filters (the server zeroes them,
  since `run_docker` ignores them and the row would otherwise assert something
  false).
- **Offline:** with the agent stopped, a page request returned **409** and wrote
  **no** `logs.page` row — the fail-closed dispatch→audit→collect ordering holds
  on the filtered path too.

**Browser checks:**
1. Logs tab streams the whole journal and defaults to current boot.
2. Changing priority or window resets the buffer and re-streams.
3. Per-unit dialog still defaults to unfiltered and pages back past a reboot.
4. A docker source shows no filter bar.

**Two defects only a browser caught** — worth remembering, because both passed
every static gate:
- The filter selects rendered the **raw value** (`5`, `all`) instead of the
  label. base-ui's `SelectValue` shows the raw value unless `Select` (Root) is
  given an `items` prop; a `{value, label}` array is then used automatically.
  A compile-only probe cannot catch this — it typechecks either way.
- The Logs tabpanel was the only one missing `mt-4`, so the filter bar sat flush
  against the tab strip.

**Known limitation (deferred):** the window is re-resolved on every request, so a
view left open longer than its own window (e.g. `window=24h` for over a day) will
find every page read fully truncated and report "beginning of window" while still
displaying a longer span. Each read is self-consistent with the current window;
the fix is for the server to return the resolved `since_ms` on stream open and
the client to echo it on page requests.

## Capability reporting + log-window fix — manual verification (2026-07-24)

Two parts: the deferred log-window bug from PR #8, then agent capability
reporting. Design of record:
`docs/superpowers/specs/2026-07-24-capability-reporting-design.md`.

### Part 1 — the log window is now one cutoff per buffer

The window used to be re-resolved on every request, so a tail and its pages
disagreed and a view open longer than its own window paged into nothing. Now
`logs/stream` resolves once and announces it in a leading **named** SSE frame,
and the client echoes it back as `since_ms`:

```
$ curl -sN ".../logs/stream?source=journal:@system&tail=2&follow=false&window=1h" | head -3
event: meta
data: {"since_ms":1784903548983}
```
The frame is **named** on purpose: a browser's `EventSource` routes named events
to `addEventListener` and *not* to `onmessage`, so it can never be parsed as a
log line by the NDJSON client code.

- An explicit `since_ms` on a page read is honoured (every line at or after it);
  `since_ms=abc` and `since_ms=-5` are both **400**.
- **`since_ms=0` must NOT override a boot window.** `window=boot` resolves to
  `since_ms=0, current_boot=true`, so the client echoes `0` back — and treating
  that as an authoritative cutoff silently dropped `-b` from the page argv.
  Measured on a host booted 2026-07-18: `window=boot` returned 0 older lines,
  while `window=boot&since_ms=0` returned 50 reaching back to 2026-07-17 — i.e.
  the *previous boot*, in a view labelled "current boot". `0` now means unset,
  matching the convention used everywhere else.
- The client adopts the announced cutoff only **once per buffer**. An
  `EventSource` auto-reconnect re-announces a fresher cutoff, but the line buffer
  is not cleared on reconnect, so adopting it would pair a new cutoff with old
  lines and resurrect the original symptom.

### Part 2 — capability reporting

The agent probes systemd (D-Bus), docker (a real `ping`) and journald
(`journalctl --version`) once per session and reports them on `AgentInfo`.

**The docker probe must be a `ping`, not `inner.is_some()`** — verified
non-destructively by giving the agent a private mount namespace whose
`/var/run/docker.sock` is a plain file (stopping dockerd was not an option: the
dev Postgres runs in Docker):

```bash
touch /tmp/notasock
sudo -n unshare -m sh -c 'mount --bind /tmp/notasock /var/run/docker.sock && exec ./target/debug/argus-agent'
```

| Condition | Reported |
|---|---|
| dockerd answering | `["systemd","docker","journal"]` |
| socket present, not answering | `["systemd","journal"]` — docker correctly dropped |

The bollard client *constructs* fine against the dead path, so `inner.is_some()`
would have wrongly claimed `docker`. Host dockerd and Postgres were verified
unaffected afterwards.

**The tri-state, through the API** (this is the whole feature):

| DB | API | UI |
|---|---|---|
| `NULL` | `null` | gate **nothing** (agent predates the feature) |
| `{}` | `[]` | gate everything (bare host, e.g. an Alpine LXC) |
| `{systemd}` | `["systemd"]` | gate the rest |

`NULL` and `{}` must never collapse: treating "not reported" as "supports
nothing" would blank every tab on a working machine the moment an older agent
connects. proto3 cannot tell an empty repeated field from an absent one, which
is why `AgentInfo.capabilities_reported` (field 9) exists.

**Capabilities refresh per session, not per heartbeat.** `Hello` carries them and
is sent at session open, so a manual `UPDATE machines SET capabilities=...`
persists until the agent reconnects. Both write paths use
`coalesce($n, machines.capabilities)` so a silent agent never *erases* an
established set, while an intentional `{}` still overwrites.

**Gating covers per-row affordances, not just tabs.** The Units tab is gated on
`systemd` but its per-unit Logs button opens a `journal:` source — a *different*
capability — so it is gated separately. The container equivalent is safe by
construction (its link needs `docker`, and the Containers tab already requires
it).

### Two operational gotchas

- **`cargo test --workspace -- --ignored` against the dev database rotates the
  CA.** One of the ignored server tests is
  `ca::load_or_init_persists_and_reloads_the_same_ca`, which rewrites
  `ca_material`. The control plane then logs *"generated and persisted new CA"*
  and every enrolled agent is orphaned (mTLS fails, machine goes offline).
  Re-enroll with a fresh token and data dir afterwards.
- **`argus-pg` does not restart itself.** If `cargo clippy`/`test` suddenly fails
  with `error communicating with database: Connection refused`, the container has
  stopped — `docker start argus-pg`. Nothing is wrong with the code; `sqlx`'s
  compile-time query checking needs a reachable database (or `SQLX_OFFLINE=true`).

## Terminal (PTY) slice — manual verification (2026-07-25)

Interactive shell to a guest: xterm.js ↔ WebSocket ↔ server ↔ the single mTLS
gRPC `Session` ↔ `portable-pty` on the agent. Design of record:
`docs/superpowers/specs/2026-07-24-terminal-design.md`. Agent runs as root.

### Running it in dev

The `/api` vite proxy **must** use the object form with `ws: true`
(`frontend/vite.config.ts`). The shorthand string form does not forward
WebSocket upgrades, so `/api/machines/:id/terminal` silently never opens. It
presents as a broken terminal, not a broken proxy: xterm does not echo locally
(the remote PTY does), so a dead socket shows **no prompt and no response to
typing**. Diagnose by comparing the two:

```bash
# opens on :8080 but times out through :5173  => the proxy, not the terminal
node -e 'const w=new WebSocket("ws://localhost:8080/api/machines/<ID>/terminal");
         w.onopen=()=>console.log("direct OPEN");w.onerror=()=>console.log("direct FAIL")'
```

### Protocol-level checks (all green)

- Real shell over a real WebSocket: `echo PTY-OK-$((6*7))` returned `PTY-OK-42`
  — the shell genuinely evaluated the arithmetic, so it is a true PTY.
- `exit` closed the socket cleanly; **zero zombies**, no orphaned shells, and the
  agent had no leftover children.
- `audit_log` gained `terminal.open` / `ok` with `target_ref` = the session UUID.
- A plain `GET` to the terminal route returns **400** (not-an-upgrade), not 404.

### The firehose test — use a SLOW consumer

**A throughput test cannot detect a backpressure bug.** A client that drains as
fast as it can never fills the server's per-session buffer, so flow control is
never exercised. The first firehose test here passed on `seq 1 200000` while the
same command in a browser killed the session with `code 1005` — xterm renders,
so it drains far slower.

The test must therefore be a deliberately slow consumer. Busy-wait a few ms per
message so the TCP receive window fills and the server's buffer crosses
`PTY_HIGH_WATER`:

```js
ws.onmessage = (e) => {
  bytes += e.data.byteLength;
  out += dec.decode(new Uint8Array(e.data), { stream: true });
  const t = Date.now(); while (Date.now() - t < 3) {}   // ~3ms of "rendering"
};
```

Expected with `seq 1 200000` (verified): the session **stays open**, all
1,488,964 bytes arrive with the final line `200000` present (**no dropped
bytes**), the machine stays `online` with fresh heartbeats throughout, and no
zombies or leftover children remain. A tear-down with `code 1005` means the
message cap is binding before the byte watermark — see the sizing rule below.

### The sizing rule (this caused a Critical)

`PTY_CHANNEL_CAP` is in **messages**; the water marks are in **bytes**. The cap
must never bind first:

> `PTY_CHANNEL_CAP × (smallest sustained chunk) > PTY_HIGH_WATER`

Real PTY chunks measure **~138 bytes** (1,488,985 bytes over 10,818 frames), not
the 64 KiB an earlier draft assumed. At `PTY_CHANNEL_CAP = 32768` and
`PTY_HIGH_WATER = 1 MiB` the breakeven is **32 bytes/chunk**, so measured traffic
sits ~4.3× above the floor. **Any change to these constants must re-check that
inequality**, not just make the numbers bigger.

### Browser checks (confirmed by the maintainer)

1. `top` redraws cleanly and `q` exits — escape sequences survive the whole chain.
2. **Maximize keeps the same session** (scrollback intact, `top` still running);
   Restore returns. A remount here would silently kill the shell.
3. Resize reflows the shell (`echo $COLUMNS`, `stty size`).
4. Switching tabs away kills the session **by design** (unmount → `PtyClose` →
   agent kills the shell).
5. Escape reaches the shell — it does **not** restore from maximize. Restoring on
   Esc would mean intercepting it, and Esc is load-bearing in vim/less/fzf.

### Two dark-mode traps

Both of these shipped looking correct in light mode and passing `tsc` + build:

- The terminal surface is always `bg-black` and the dark theme's page background
  is `#000000`, so without a border the terminal is **invisible** against the
  page. `--border` (`#242424` dark / `#D4D4D8` light) works for both.
- The border **and** the padding must sit on the *wrapper*, not the div xterm
  mounts into. `FitAddon` measures `term.element.parentElement`'s computed
  height, which under this app's `border-box` reset includes that element's own
  border and padding — putting either on the mount div makes it overcount and
  clip the last row.

**Check every UI change in both themes.** This is the third dark-mode-only defect
in this project (after the tab colour and the log viewer's dark-on-dark text).

### Known gaps (recorded in the design doc)

WebSocket Pongs are not tracked, so the keepalive Ping is a liveness probe rather
than dead-socket detection — the 30-minute idle timer is the real backstop. Also
deferred: agent-side read coalescing, and a bounded PTY input queue.

## Machine status: a heartbeat restores `online` (2026-07-25)

**The bug.** `mark_stale_offline` flips any machine whose `last_seen_at` falls
behind the 45s cutoff — including one whose session is still up and merely
*stalled* (a slow host, a paused VM, a network hiccup). The heartbeat path then
only stamped `last_seen_at` and deliberately left `status` alone, and the only
other writer of `online` was a brand-new session. So a machine swept during a
stall could never come back: heartbeats resumed and `last_seen_at` ticked up
every interval while the fleet page showed it `offline` indefinitely.

The symptom is distinctive, and worth recognising — **`last_seen_at` advancing
while the badge says offline**. A machine that is genuinely gone has a frozen
`last_seen_at`; only this bug produces a fresh timestamp on an offline row.

**The fix.** `repo::touch_last_seen` is gone; the liveness-bearing frames
(`Heartbeat`, `Metrics`, `DockerState`, `SystemdState`) call `repo::mark_online`,
which stamps the timestamp *and* re-asserts the status. A frame arriving on an
authenticated session is itself the proof of life, so it is what restores the
status. `LogChunk` still deliberately does not count — a busy log would otherwise
mask a wedged agent.

**Live verification.** With an agent connected, force the sweeper's effect and
watch a heartbeat undo it:

```bash
Q() { docker exec argus-pg psql -U postgres -d argus -tAc "$1"; }
Q "SELECT count(*) FROM audit_log WHERE action='agent.online'"   # note it
Q "UPDATE machines SET status='offline' WHERE hostname='fatman'"
sleep 35
Q "SELECT status FROM machines WHERE hostname='fatman'"          # -> online
Q "SELECT count(*) FROM audit_log WHERE action='agent.online'"   # -> UNCHANGED
```

The audit count is the load-bearing part of this check. `agent.online` is written
only on `Hello`, so an unchanged count proves no new session occurred and the
recovery came from a heartbeat on the *existing* stream — which is the thing that
was broken. Measured: `offline` at 10:18:02 → `online` by 10:18:09, audit count
34 before and after.

The regression test is `repo::tests::heartbeat_after_sweep_restores_online`. It
sets its precondition in raw SQL rather than by calling `mark_online`: driving
setup through the function under test made the deliberate-break check fail at the
precondition (the sweep flipped nothing, because nothing was ever `online`)
instead of at the assertion that names the bug.

> Re-verifying this rotates nothing, but note that running the gates *before* it
> does: `cargo test --workspace -- --ignored` rewrites `ca_material` (see the
> gotcha above), so re-enroll the agent before trying the live check.

## OIDC authentication — dev setup + live verification checklist (2026-07-25)

Every browser surface (`/api/*`, including the SSE log streams and the terminal
WebSocket — cookies ride the upgrade request, so one middleware layer covers all
three transports) now sits behind a signed-in, revocable, Postgres-backed
session. Design of record: `docs/superpowers/specs/2026-07-25-oidc-design.md`.
Agents are entirely unaffected: they authenticate by mTLS on the separate agent
gRPC listener and never touch this path.

### There is no "auth disabled" mode, by design

`crates/server/src/config.rs::Config::from_env` reads every required OIDC field
through the same `req()` helper as `ARGUS_DATABASE_URL`/`ARGUS_FIELD_KEY` — **the
control plane will not boot** without `ARGUS_OIDC_ISSUER`,
`ARGUS_OIDC_CLIENT_ID`, `ARGUS_OIDC_CLIENT_SECRET`, `ARGUS_OIDC_REQUIRED_ROLE`,
and `ARGUS_PUBLIC_URL` all set. There is no dev-only bypass flag and no
unauthenticated fallback: local development authenticates against a real
provider exactly like production does (design doc §5.1).

### Break-glass: recovering from a first-boot OIDC misconfiguration

Because there is no auth-disabled mode, a misconfiguration you can't fix from
the provider side alone — a trailing-slash `iss` mismatch, a redirect URI that
was never registered, or the wrong `ARGUS_OIDC_ROLES_CLAIM` denying every
account — locks the maintainer out of their own control plane with no
in-product recovery. This is an **emergency recovery path that requires direct
database access, not a routine one.** If you can fix the provider-side
configuration or an env var instead, do that.

The `sessions` table (`crates/server/migrations/0004_sessions.sql`) is:

```sql
create table sessions (
    token_hash   bytea       primary key,  -- sha256 of the raw cookie value
    subject      text        not null,
    email        text,
    display_name text,
    created_at   timestamptz not null default now(),
    expires_at   timestamptz not null
);
```

Only the SHA-256 of the cookie is ever stored, so recovery means picking a raw
token yourself, inserting its hash, and then setting a browser cookie to that
same raw value — the same idiom used for enrollment tokens and for live check
7 above. Concretely:

```bash
# 1. Pick a raw token (anything unguessable) and hash it:
TOKEN=$(head -c 32 /dev/urandom | base64 | tr -d '=+/')
HASH=$(printf '%s' "$TOKEN" | sha256sum | cut -d' ' -f1)

# 2. Insert a session row directly, expiring it far enough out to get the
#    misconfiguration fixed (here, 1 hour):
docker exec argus-pg psql -U postgres -d argus -c \
  "INSERT INTO sessions (token_hash, subject, email, display_name, expires_at)
   VALUES (decode('$HASH','hex'), 'break-glass', 'break-glass@local', 'Break-glass',
           now() + interval '1 hour');"

# 3. Print the raw token — this is the cookie VALUE, not the hash:
echo "$TOKEN"
```

Then, in the browser, set a cookie named `argus_session` (`argus_common::SESSION_COOKIE`)
on the control plane's origin to that raw `$TOKEN` value (devtools → Application/Storage
→ Cookies → add one), and reload. That resolves to the `sessions` row via the same
`token_hash` lookup every other request uses, so `/api/*` — including `/api/me` — now
authenticates as the `break-glass` identity, which is enough to reach the UI and
diagnose the real problem (e.g. read the `available_claims` WARN log described below).

Delete the row once you're done (`DELETE FROM sessions WHERE subject = 'break-glass';`)
rather than letting it ride out its `expires_at` — it is a credential with no real
identity behind it and should not outlive the incident.

### Required configuration

| Variable | Example (dev) | Meaning |
|---|---|---|
| `ARGUS_OIDC_ISSUER` | `https://auth.lab.example.com` | Must match the provider's `iss` claim **exactly** — a trailing-slash mismatch is the classic discovery failure. |
| `ARGUS_OIDC_CLIENT_ID` | `argus-dev` | Client ID of the Argus app registration at the provider. |
| `ARGUS_OIDC_CLIENT_SECRET` | *(issued by the provider)* | Confidential-client secret. |
| `ARGUS_OIDC_REQUIRED_ROLE` | `argus-admin`, or the literal `any` | Role required for admission. `any` must be typed explicitly (§5.2) — an **unset** variable does not mean "open to everyone", it means the server refuses to start. |
| `ARGUS_PUBLIC_URL` | `http://localhost:8080` | Externally reachable base URL. Decides the session cookie's `Secure` attribute (`https://` → set) and builds the exact `redirect_uri` (§5.3); deliberately never derived from `Host`/`X-Forwarded-Proto`, which a client controls. |

Optional:

| Variable | Default | Meaning |
|---|---|---|
| `ARGUS_OIDC_ROLES_CLAIM` | `groups` | Dot-path into the merged claims object — see below for finding the right value per provider. |
| `ARGUS_OIDC_SCOPES` | `openid profile email` | Space-delimited scopes; some providers need an extra one before they emit roles at all. |
| `ARGUS_OIDC_CA_CERT` | *(unset)* | PEM path, for an IdP behind an internal CA (common in a homelab). |

Session lifetime (`SESSION_TTL_HOURS = 12`, hours) and the cookie name
(`SESSION_COOKIE = "argus_session"`) are constants in `argus-common`, not
environment configuration — a security property of the product, not a
per-deployment knob.

### Registering the two dev redirect URIs

Argus builds exactly one `redirect_uri`: `<ARGUS_PUBLIC_URL>/auth/callback`.
Local dev has two distinct origins depending on how the frontend is served, so
the provider's app registration needs **both** as allowed redirect URIs:

- `http://localhost:8080/auth/callback` — running the server directly
  (`cargo run -p argus-server`), which serves the built `frontend/dist`.
- `http://localhost:5173/auth/callback` — running the frontend under
  `npm --prefix frontend run dev` (Vite), which proxies `/api` and `/auth` to
  `:8080`.

An unregistered redirect URI fails either the initial authorize redirect or the
token exchange (provider-dependent) with an error specific to that provider —
if login fails immediately at the provider's own page, check this first.

### The `/auth` vite dev-proxy entry

`frontend/vite.config.ts`'s dev `server.proxy` carries **both** `/api`
(object form, `ws: true` — the terminal-slice trap documented above; the string
shorthand silently drops WebSocket upgrades) and `/auth`. Without the `/auth`
entry, Vite's SPA fallback serves `index.html` for `/auth/callback` instead of
proxying it to the control plane, so the login flow appears to silently bounce
back into the bare app shell instead of erroring — both entries are already
wired; keep both if that file is ever rewritten.

### Finding the right `ARGUS_OIDC_ROLES_CLAIM` for your provider

A misconfigured claim path is otherwise indistinguishable from a code bug —
login succeeds, but every account is denied. On a role denial the server logs a
`WARN` naming the claim **keys** it actually saw (never values, since those can
carry emails and other personal data) and the roles it extracted
(`crates/server/src/auth/oidc.rs`, around the `is_admitted` check):

```
WARN login denied: required role not held
    subject=<sub> required=Named("argus-admin") roles_claim="groups"
    extracted_roles=[] available_claims=["sub","email","name","urn:zitadel:iam:org:project:roles",...]
```

Read `available_claims` for the key that actually holds roles on your provider,
set `ARGUS_OIDC_ROLES_CLAIM` to it (dot-path if the provider nests it, e.g.
Keycloak), restart, and try again — `extracted_roles` on the next denial (or
success) confirms whether the path resolved. The four provider shapes the
extractor is unit-tested against (design doc §4.1):

| Provider | `ARGUS_OIDC_ROLES_CLAIM` | Claim shape |
|---|---|---|
| Keycloak | `realm_access.roles` | Array of strings, nested one level under `realm_access`. |
| Authentik, Okta | `groups` (the default) | Flat array of strings. |
| Zitadel | `urn:zitadel:iam:org:project:roles` | **Object** whose *keys* are the role names (values are per-org metadata) — the extractor reads the keys. |
| Auth0 | a namespaced custom claim, e.g. `https://argus.example.com/roles` | Usually an array; namespaced because Auth0 refuses to emit un-namespaced custom claims. |

Claims are merged — userinfo over the ID token — **before** the path is
resolved (§4.2), so if the target claim shows up in `available_claims` at all,
the merge already happened; a still-empty `extracted_roles` means the
configured dot-path just doesn't match what the log shows.

### CA rotation gotcha applies here too

The full gate suite (`cargo test --workspace -- --ignored`, part of the sequence
below) rotates the dev CA and orphans the enrolled agent — see "Two operational
gotchas" above; this is the same rotation, not a new one. Re-enroll the agent
before running live check 9 below (agent connectivity surviving an IdP outage),
or that check observes a machine offline for the wrong reason.

### Full gate run (2026-07-25)

Run against the dev Postgres (`argus-pg`), on the `oidc-slice` branch, after
wiring `repo::delete_expired_sessions` into the `jobs::run` sweeper tick:

```
$ npm --prefix frontend run build            # exit 0
$ cargo fmt --all --check                    # exit 0, clean
$ SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings   # exit 0, clean
$ cargo sqlx prepare --workspace --check -- --all-targets                  # exit 0, clean (query unchanged since Task 3)
$ cargo test --workspace
  argus-agent:  64 passed, 0 failed, 10 ignored
  argus-common:  0 passed, 0 failed,  0 ignored
  argus-proto:   0 passed, 0 failed,  0 ignored
  argus-server: 133 passed, 0 failed,  6 ignored
$ cargo test --workspace -- --ignored --skip live_
  argus-server:  6 passed, 0 failed  (includes ca::tests::load_or_init_persists_and_reloads_the_same_ca — rotates ca_material, see above)
  argus-agent/argus-common/argus-proto: 0 run (agent's ignored set is entirely live_-prefixed and filtered out, matching the CI gate)
```

All gates clean. One non-blocking observation: `npm run build` warns that the
main chunk (`index-*.js`, 1,245.52 kB / gzip 378.92 kB) exceeds
`build.chunkSizeWarningLimit` (750 kB, set by the frontend-design-system slice).
That growth predates this task (already present on `oidc-slice` before this
commit — the sign-in gate and TanStack Query auth plumbing from Task 7) and is
a warning, not a build failure; revisit the chunk-size budget in a future
frontend pass if it keeps growing.

### Live run against Zitadel — 2026-07-25 (partial)

First execution of the flow against a real provider (`https://auth.nexus.e412.in`).
**Only the rows listed here were measured; the rest of the checklist below is
still pending.**

| §12 check | Result |
|---|---|
| 1 — log in through the real provider | **PASS.** Landed back on the fleet page signed in. |
| 3 — a verb's audit row carries the identity | **PASS.** A `unit.restart` dispatched against a deliberately non-existent unit recorded `actor = verify@example.test` (a temporary session, since deleted), target `argus-nonexistent-check.service`, result `error`. The bogus target keeps the check side-effect-free while still exercising the actor path. |
| 5 — sign out | **PASS.** `auth.logout` row written; `/api/*` then 401s. |
| 9 — an IdP outage must not touch the agent surface | **PASS.** With `ARGUS_OIDC_ISSUER` pointed at an unreachable address (`https://127.0.0.1:9`), the control plane **booted normally** — proving discovery is lazy rather than fetched at startup — the agent stayed `online` with `last_seen_at` advancing across 40s (19:49:29 → 19:49:52 → 19:50:07), and `GET /auth/login` returned `503` gracefully rather than hanging or crashing. Restoring the real issuer restored `303`. |
| 8 — role admission and denial | **PASS, both directions.** With `ARGUS_OIDC_REQUIRED_ROLE=argus`: the account holding the role was admitted (`auth.login | ok`), the account without it was denied (`auth.denied | denied`). Earlier, before Zitadel asserted roles at all: With `ARGUS_OIDC_REQUIRED_ROLE` set to a role the account lacks, login was denied with the explicit "lacks the required role" message, `auth.denied` recorded `{"subject": …}` in `detail`, and the WARN log printed `required`, `roles_claim`, `extracted_roles=[]` and `available_claims=["client_id","sid"]` — claim **names** only, no values. |
| (gate) unauthenticated `/api/*` | **PASS.** `/healthz` 200, `/api/fleet` 401, `/api/me` 401, both direct and through the vite proxy. |
| (audit) actor identity | **PASS.** `auth.login` recorded with `actor = <email>`, not `anonymous`. Session row carried subject, email and display name; TTL landed at 12h. |

Not yet measured: 2, 4, 6, 7 — all minor, and each covered by an automated test.

### Getting Zitadel to actually emit roles

This took roughly an hour to pin down and every wrong turn looked exactly like an
Argus bug, so the conclusion is recorded in full.

**The one setting that matters is at the PROJECT level.** Zitadel emits no roles
until **Project → General → "Assert roles on authentication"** is enabled. The
application's **Token settings → "User roles inside ID token"** is *not*
sufficient on its own — it governs where roles are placed once they are being
asserted, not whether they are asserted at all. With only the app-level toggle
on, the claim is absent from the ID token *and* from userinfo.

**Things that are NOT required, tested and confirmed:**

- The reserved scope `urn:zitadel:iam:org:project:roles`. It was added on a
  hunch, changed nothing, and after project assertion was enabled the login
  worked with it removed. Leave `ARGUS_OIDC_SCOPES` unset.
- Anything in Argus. `ARGUS_OIDC_ROLES_CLAIM = urn:zitadel:iam:org:project:roles`
  is correct for Zitadel throughout.

**A user with no roles gets the claim omitted entirely**, not sent empty. So an
absent claim means either "assertion is off" or "this user holds no roles", and
the two are indistinguishable from the token alone — check the project setting
first, then the user's grant.

**Beware the SSO session when testing.** A private window gives a fresh *Argus*
session but Zitadel keeps its own. Once signed in there, every subsequent
`/auth/login` silently returns the same identity with no account picker, so
"retrying with the other account" quietly re-tests the first one. Three
consecutive runs were wasted this way, and the only reason it surfaced was the
`subject=` field in the denial log. **Sign out of Zitadel itself**, not just
Argus, when switching accounts.

**Two users can share a display name.** The `subject` in the log is the only
reliable identifier; the Zitadel console's user list is not.

**Verified end to end, both directions:** with `ARGUS_OIDC_REQUIRED_ROLE=argus`,
the account holding the role logged in (`auth.login | ok | <email>`) and the
account without it was denied (`auth.denied | denied | <subject>`).

Until roles are asserted, `any` is the correct setting, and it is safe precisely
because it is explicit: an unset variable refuses to boot rather than admitting
everyone.

**This run found a real bug that no amount of review had, and it is the reason
the live gate is not a formality.** Zitadel's ID token names more than one
audience — this client plus a sibling application from the same project — and
`openidconnect` correctly rejected it (`Invalid audiences: ... is not a trusted
audience`), because OIDC Core 3.1.3.7 requires a client to reject audiences it
does not trust.

The obvious fix, trusting extra audiences, is a trap. It is safe only if the
`azp` (authorized party) claim is verified to be this client — and
`openidconnect` 4.0.1 **does not verify `azp`**: that code is commented out in
`verification/mod.rs` behind a note deferring it "until a use case becomes
apparent". Relaxing the audience check alone would therefore not have relocated
the guarantee, it would have deleted it: `aud` would no longer distinguish a
token minted *for* Argus from one minted for any other application in the same
Zitadel that merely happens to list Argus. The fix does both — accept additional
audiences, **and** implement the `azp` check the library skipped, scoped to the
multi-audience case the spec requires it for. Regression test:
`auth::oidc::tests::multi_audience_tokens_require_azp_to_be_this_client`.

Two configuration notes from the same run:

- **The issuer must not carry a trailing slash.** The discovery document reports
  `"https://auth.nexus.e412.in"`; `.../` fails discovery in a way that reads
  like a code bug.
- **`ARGUS_PUBLIC_URL` must be the origin the *browser* uses**, not the server's
  loopback. Reaching the dev UI over the LAN means `http://<lan-ip>:5173`, and
  that exact `redirect_uri` must be registered at the provider.

### Live verification checklist (design doc §12) — REMAINING ROWS PENDING

**Not yet run against a real provider.** Everything above this line was
verified: unit tests, `axum` router `oneshot` tests, and `sqlx::test` database
tests, none of which touch a live IdP. The nine checks below need a real OIDC
issuer, a registered client ID/secret, and access to that provider's admin
console (to toggle a test account's roles and to stop/start the service) — none
of which were available for this pass. **No row below has been measured. Do not
treat any of them as passing, and do not fill in an "observed" value, until a
maintainer has actually run it.**

Prerequisites: export the five required variables above against a real
provider, register both dev redirect URIs at the provider, then
`cargo run -p argus-server`.

| # | Check | How | Expected |
|---|---|---|---|
| 1 | Login round-trip | Visit `http://localhost:8080`, click sign-in, complete login at the provider. | Redirected back to the fleet page, signed in. |
| 2 | `/api/me` | Check the network tab after step 1, or `curl -b <cookie jar> http://localhost:8080/api/me`. | `200` with `{"subject": "...", "email": "...", "display_name": "..."}` matching the account used to log in. |
| 3 | Audit carries the real identity | Run any verb (container or unit action) while signed in, then `SELECT actor FROM audit_log ORDER BY id DESC LIMIT 1;`. | `actor` is the account's **email**, never the literal `anonymous`. |
| 4 | Terminal WebSocket carries the cookie | Open a machine's terminal tab in the *browser* (not curl — the cookie must ride the upgrade request). | The shell opens and is interactive, proving the WS upgrade carried `argus_session`. |
| 5 | Sign-out | Click "Sign out" (sidebar footer), then let the SPA refetch, or `GET /api/fleet` directly. | `POST /auth/logout` → `204`; the next `/api/fleet` → `401`; the SPA renders the sign-in view. |
| 6 | Tampered cookie is rejected | In devtools, flip one character of the `argus_session` cookie value, then hit any `/api/*` route. | `401 {"error":"unauthenticated"}`. |
| 7 | Expiry is enforced server-side, not just client-side | While signed in, copy the `argus_session` cookie value from devtools and expire that row directly (same `sha256`-then-`decode(...,'hex')` idiom the enrollment step above uses): `HASH=$(printf '<raw cookie value>' \| sha256sum \| cut -d' ' -f1); docker exec argus-pg psql -U postgres -d argus -c "UPDATE sessions SET expires_at = now() - interval '1 hour' WHERE token_hash = decode('$HASH','hex');"` — then hit any `/api/*` route. | `401` on the very next request — the cookie itself is untouched, so this proves expiry isn't a `Max-Age` artifact. |
| 8 | Role denial | Set `ARGUS_OIDC_REQUIRED_ROLE` to a role the test account does **not** hold, restart, attempt login. | Login is denied with the explicit "does not have the role required" error page (§14) rather than a generic failure; `SELECT * FROM audit_log WHERE action='auth.denied' ORDER BY id DESC LIMIT 1;` shows a fresh row, the rejected subject in `detail`. |
| 9 | **IdP outage does not take the fleet down** — the check most likely to regress silently | With an already-enrolled agent connected, stop the OIDC provider (or block the network path to it), then restart Argus. Watch `/api/fleet` and the agent's own logs. | The control plane boots — only configuration *presence* is validated at startup, never connectivity; discovery is lazy and cached (§10). The agent reconnects over mTLS and heartbeats normally; `/api/fleet` shows it `online`. **Only** `/auth/login` fails (discovery/token requests to the unreachable provider time out or error) — nothing else is affected. |

Fill in an "Observed" note per row (or add a new dated verification section
below this one, matching the pattern used elsewhere in this file) once a
maintainer runs these against the real provider — do not edit this table to
claim a result that wasn't measured.

## Local admin (break-glass) — dev setup + live verification (2026-07-26)

Design of record: `docs/superpowers/specs/2026-07-26-local-admin-design.md`.
This is the recovery path for the OIDC slice above: the boot rule becomes
"OIDC is configured **or** a local admin row exists" (design §4), so a lost
client secret, a deleted IdP application, or a from-scratch deployment with no
IdP at all no longer means the control plane refuses to start.

### The CLI: `argus local-admin reset`

```
$ ARGUS_DATABASE_URL='postgres://postgres:argus@localhost:5432/argus' \
    ./target/debug/argus local-admin reset
Local admin created.

  username: admin
  password: <24 random characters>

This password is shown ONCE and is not recoverable. Store it now.
Run this command again to issue a new one.
```

- Reads **only `ARGUS_DATABASE_URL`**, not the full `Config::from_env` — it
  deliberately does not require the OIDC variables, `ARGUS_FIELD_KEY`, or
  `ARGUS_PUBLIC_URL`, because a recovery command that needs the configuration
  that broke is not a recovery command (design §5.1). It works with the
  server **stopped**, connecting to Postgres directly.
- The password is **always generated** (24 chars, crypto-secure RNG), never
  chosen, and is printed **once**. There is no "set this specific password"
  path and no way to display it again — rerunning the command issues a new
  one and immediately invalidates the old one (single-row table, `upsert`).
- The same generate-hash-store routine backs the in-app rotation control
  (`POST /api/local-admin/rotate`, authenticated, reachable while signed in by
  either method) — there is exactly one implementation of "rotate", not two
  that could drift.
- There is deliberately **no unauthenticated setup page**: a page that can
  create an admin whenever the table is empty is a takeover vector the moment
  it re-arms, and it re-arms exactly when a restore-from-backup or a botched
  migration would also leave the table empty (design §5.3). The CLI is the
  only way to populate the table from a "nobody can sign in" state.

### The boot rule

`auth_is_configured` (`crates/server/src/main.rs`) refuses to boot only when
**both** are missing: no `ARGUS_OIDC_*` config **and** no `local_admin` row.
Either one alone is enough to start. The refusal names the fix:

```
Error: no authentication is configured: set the OIDC variables, or create a local admin with `argus local-admin reset`
```

### In-app rotation provisions the credential, not only rotates it

`POST /api/local-admin/rotate` (authenticated, reachable while signed in by
either method) calls `reset_local_admin` — the same function the CLI calls —
and that function does not require a `local_admin` row to already exist: when
the table is empty, rotation *creates* the account (falling back to username
`admin`) and hands the caller its password, same as if it had rotated an
existing one.

In an OIDC-only deployment, that means any signed-in user can mint a
break-glass local credential that did not exist before — and the resulting
password keeps working after that user is removed from the identity provider,
through a path the IdP cannot revoke. This is accepted behaviour (design
§15), not a defect: the action is authenticated and audited
(`local_admin.rotate` names the actor via `Actor::User`), and it is no worse
in kind than rotating an existing row. `local_admin.rotate` in the audit log,
plus `last_login_at`, are how you detect its use.

### Signing in with no identity provider configured at all

Unlike OIDC, `POST /auth/local` is a plain JSON endpoint — no browser redirect
dance, so `curl` exercises the whole feature:

```bash
curl -c cookies.txt -X POST http://127.0.0.1:8080/auth/local \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"<the generated password>"}'
# 200 {"ok":true}, Set-Cookie: argus_session=...; HttpOnly; SameSite=Lax; Max-Age=43200
```

Success mints a session through the **same `create_session`/`argus_session`
cookie** the OIDC callback uses, with `subject = "local:admin"` (the `local:`
prefix can never collide with a provider's `sub`). `/api/me` and `/api/fleet`
behave identically to an OIDC-sourced session — there is no second session
concept, and the browser surface's `require_auth` middleware, expiry, and
logout all work unchanged.

**In the browser**, the same credentials go in via `frontend/src/components/SignIn.tsx`:
open the app, and beneath the SSO "Sign in" button there is a collapsed **"Use
a local account"** disclosure — click it to reveal the username/password
fields, then submit the credentials from the `argus local-admin reset` output
above. This is
deliberately not the first thing on the page (SSO stays primary, design §12),
so during a real incident it is easy to glance at the sign-in screen, see only
the SSO button, and assume the recovery path isn't there — it is, one click
down. A live click-through of this exact form (not just the `curl` equivalent
above) is still worth doing once, the same way the OIDC section above has
rows it flags as not yet run against a real browser — **not done in this
pass**; everything measured in this task went through `curl` only.

### CA rotation gotcha applies here too

Same rotation as the OIDC section above ("CA rotation gotcha applies here
too" / "Two operational gotchas") — `cargo test --workspace -- --ignored`
deletes and regenerates `ca_material`, orphaning any enrolled agent. Nothing
about the local-admin slice changes that; it is not a new gotcha, just the
same one hit again by the same gate run.

### Full gate run (2026-07-26)

Run against the dev Postgres (`argus-pg`), on the `local-admin-slice` branch:

```
$ npm --prefix frontend run build            # exit 0
$ cargo fmt --all --check                    # exit 0, clean
$ SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings   # exit 0, clean
$ cargo sqlx prepare --workspace --check -- --all-targets                  # exit 0, clean (no query changes this task)
$ cargo test --workspace
  argus-agent:   64 passed, 0 failed, 10 ignored
  argus-common:   0 passed, 0 failed,  0 ignored
  argus-proto:    0 passed, 0 failed,  0 ignored
  argus-server:  172 passed, 0 failed,  6 ignored
  tests/local_admin_cli.rs (integration): 1 passed, 0 failed
$ cargo test --workspace -- --ignored --skip live_
  argus-server: 6 passed, 0 failed (includes ca::tests::load_or_init_persists_and_reloads_the_same_ca —
                                     deletes + regenerates ca_material, see the gotcha below)
  argus-agent/argus-common/argus-proto: 0 run (same as the OIDC pass — the agent's ignored set is
                                                entirely live_-prefixed and filtered out)
```

All gates clean. The pre-existing `npm run build` chunk-size warning
(`index-*.js` now 1,259.80 kB / gzip 382.95 kB, over the 750 kB
`build.chunkSizeWarningLimit`) is unchanged in kind from the OIDC pass above —
slightly larger from this task's added auth UI, still a warning not a
failure.

### Live verification (design §14) — 2026-07-26

Run in order, server stopped between the CLI step and boot, against the dev
Postgres (`argus-pg`):

1. **`argus local-admin reset`** — created `admin` / a 24-char generated
   password, printed once, as shown above.
2. **Booted with every `ARGUS_OIDC_*` variable unset** (`env -u
   ARGUS_OIDC_ISSUER -u ARGUS_OIDC_CLIENT_ID -u ARGUS_OIDC_CLIENT_SECRET -u
   ARGUS_OIDC_REQUIRED_ROLE -u ARGUS_OIDC_ROLES_CLAIM -u ARGUS_OIDC_SCOPES -u
   ARGUS_OIDC_CA_CERT`) — **PASS.** Log: `starting argus control plane` →
   `migrations applied` → `browser HTTP surface listening` /
   `agent gRPC surface listening`. No OIDC-related log line at all — this is
   a deployment with no identity provider configured, full stop.
3. **`curl -X POST /auth/local`** with the generated credentials — **PASS.**
   `200 {"ok":true}`, `Set-Cookie: argus_session=...; HttpOnly; SameSite=Lax;
   Path=/; Max-Age=43200`.
4. **That cookie against `/api/me` and `/api/fleet`** — **PASS.** `/api/me` →
   `200 {"display_name":"Local admin","email":null,"subject":"local:admin"}`;
   `/api/fleet` → `200` with the normal machine list (one machine, `offline`)
   — an ordinary session, indistinguishable from an OIDC one to the
   middleware.
5. **Audit row** —
   `SELECT actor, action, result, detail FROM audit_log WHERE action LIKE 'auth.%' ORDER BY id DESC LIMIT 3`
   — **PASS.** Freshest row: `local:admin | auth.login | ok | {"method":
   "local"}`. (The two rows beneath it were pre-existing `auth.denied` entries
   from earlier manual testing that pre-dates this run, not from this
   sequence — included here because the query is a plain `LIMIT 3`.)
6. **Rotation invalidates the previous password** — **PASS.** Rotated via the
   authenticated `POST /api/local-admin/rotate` (not the CLI, to exercise the
   in-app path too) while signed in with the session from step 3; retried
   `/auth/local` with the *old* password → `401
   {"error":"invalid username or password"}`; retried with the *new* password
   → `200 {"ok":true}`.
7. **Boot refuses with `local_admin` empty and OIDC unset, naming the CLI** —
   **PASS.** `DELETE FROM local_admin;` (0 rows left), then ran the compiled
   binary directly (`./target/debug/argus`, same env as step 2) — exited
   `1` immediately after `migrations applied`, before touching the CA or
   opening either listener, with:
   `Error: no authentication is configured: set the OIDC variables, or create a local admin with \`argus local-admin reset\``.
   One tooling wrinkle, not a product finding: driving this same check through
   `cargo run -p argus-server` piped into `tail` hung past a 2-minute timeout
   instead of exiting — invoking the already-built binary directly gave the
   clean, immediate exit above, so that's what's recorded as the measured
   result.

All seven steps passed. A local admin row was recreated at the end (a fresh
`argus local-admin reset`) so the environment was left with a working
break-glass credential rather than the empty table step 7 requires.

**Environment left running after this pass:** the control plane restarted on
`:8080` with every `ARGUS_OIDC_*` variable unset (same as step 2, `ca_material`
reloaded — "loaded existing CA from ca_material" — rather than regenerated,
since step 2's boot already did that regeneration once and step 7 never
touched `ca_material` again); the vite dev server on `:5173` was never
stopped. The dev agent enrolled before this task remains orphaned by the
`--ignored` gate's CA rotation (see above) and needs re-enrolling before any
agent-facing check.

## Fleet identity & navigation — live verification (2026-07-28)

Design of record: `docs/superpowers/specs/2026-07-28-fleet-identity-design.md`.
API-level pass run with curl against the dev control plane (branch build,
migration 0006 applied on startup) and the real dev agent. All green:

- **Mint** (`POST /api/enrollment-tokens`, local-admin session): body tags
  `["Dev ", " e2e", "dev"]` came back `["dev", "e2e"]` — trim/lowercase/dedupe
  happens server-side at mint. Defaults confirmed: `max_uses: 1`,
  `expires_at` ≈ now+24h, `created_by: "local:admin"`, raw `token` present in
  the 201 and in no other response.
- **Enroll-time identity** (the Task 5 seam, live): killed the dev agent,
  wiped `ARGUS_DATA_DIR`, re-enrolled with the minted token → the SAME
  machine row (keyed on `machine_id`) picked up `display_name "Fatman (dev)"`
  and `tags {dev,e2e}`; token then shows `uses 1/1` ("used" in the list API).
- **PATCH** `/api/machines/{id}`: rename/retag/notes round-trip in one call;
  `{"tags": ["has space"]}` → 400 with the actionable message naming the tag;
  partial semantics verified (revert PATCH left unlisted fields alone).
- **Fleet payload** carries `display_name`, `tags`, `capabilities`.
  **`GET /api/ca.pem`** serves the CA PEM behind auth.
- **Revoke** → 204; list-state derivation shows `revoked` / `used` / `active`
  correctly across the table's real history.
- **Audit**: `machine.update` rows carry field NAMES only (never values);
  `enroll_token.create`/`enroll_token.revoke` carry the label (and, after the
  final-review fix, the token id — names are not unique).

One operational note: the enroll page (and any new `/api` route) 404-falls
through to the SPA on a control plane built before this slice — the browser
then shows "Unexpected token '<'" from parsing `index.html` as JSON. That is
a stale-binary symptom, not a routing bug: rebuild and restart `argus`.

Local admin was reset during this pass (the previous password had been
rotated during the PR #12 checks): the break-glass credential in use is the
one issued 2026-07-28 by `argus local-admin reset`.

Browser checklist (operator): identity dialog single-border both themes;
chip-count contrast inside the selected Badge; server-400 Alert in the
dialog via an invalid-charset tag; Enter-with-highlight commits the
suggestion; enroll page mint/copy/revoke flow; grouped view duplicating a
multi-tag machine; URL round-trip in a fresh tab; Ctrl+K from a cold page.
