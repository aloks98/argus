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
