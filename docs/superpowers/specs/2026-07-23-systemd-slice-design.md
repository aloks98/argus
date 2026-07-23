# Systemd slice — design

**Date:** 2026-07-23
**Build order:** slice 4 of 6 (PRD §8) — "Systemd state + unit verbs (`zbus`)".
**Depends on:** Spine (#1), Metrics (#2), Docker (#3), frontend design system (#4)
— all merged.

## Goal

End-to-end, independently testable: an agent reports its systemd unit state to
the control plane, the machine-detail page lists those units with failures
first, an operator can start / stop / restart a unit from the browser — each
verb audit-logged and its *real* outcome surfaced back to the user — and the
fleet page shows which machines have failed units.

## Scope

- **In:** unit state display, `start` / `stop` / `restart` verbs, a failed-unit
  count on the fleet page.
- **Out:** journal *log* tailing (slice 5), terminal (slice 6). `enable` /
  `disable` / `daemon-reload` / `mask` are not in the V1 proto `Verb` enum and
  are not added. User-session buses are not consulted — system bus only.

## Proto

**No proto change.** `SystemdState`, `Unit`, and `Verb {UNIT_START, UNIT_STOP,
UNIT_RESTART}` already exist in `crates/proto/proto/argus.proto`. Like the
Docker slice, this fills in behavior around a contract that is already the
source of truth.

## Decisions

- **Reported set = loaded `.service` units + any failed unit of any type.** A
  host has ~400 loaded units; sending all of them every 15s is ~40KB per frame
  per host for rows nobody reads. Restricting to services alone would hide a
  failed `.mount` or a dead `.timer`, which is exactly the thing Argus exists to
  make visible without SSH. The union is typically 40–90 rows.
- **Filtering happens on the bus, not in the agent.** Two
  `ListUnitsByPatterns` calls — `(states: [], patterns: ["*.service"])` and
  `(states: ["failed"], patterns: [])` — unioned and deduplicated by unit name.
  systemd does the filtering, so the D-Bus payload is already small when it
  reaches us. `ListUnitsByPatterns` has existed since systemd 230 (2016), well
  below the floor of any guest in this fleet (Debian 12, Flatcar).
- **Verb success means the job finished, not that it was enqueued.**
  `StartUnit` returns as soon as systemd *queues* a job; a unit whose
  `ExecStart` then fails would still report `ok`. The agent therefore does what
  `systemctl start` itself does: subscribe to `JobRemoved`, correlate by the job
  object path returned from the call, and report systemd's own result string
  (`done` / `failed` / `timeout` / `canceled` / `dependency` / `skipped`).
  Bounded by a **90s** job timeout — not the 5s `docker.rs` uses. That 5s
  bounds a daemon *round trip*; this bounds a systemd **job**, and systemd's own
  `DefaultTimeoutStartSec` is 90s, so `docker.service`, `postgresql.service`, or
  anything with an `ExecStartPre` would routinely blow a 5s cap and report
  `ok: false` for a unit that started perfectly — eroding trust in the `ok` field
  from the opposite side. The control plane's own 10s wait independently returns
  `202 pending` to the browser, and the late `CommandResult` still resolves the
  audit row, so waiting longer here costs the user nothing; verbs already run in
  a spawned task, so heartbeats are unaffected. Listing keeps the 5s bound. On
  timeout the result is `ok: false` with "job enqueued; outcome unconfirmed",
  never a false `ok`.
  - **Ordering matters:** the signal stream is opened *before* `StartUnit` is
    called, otherwise a fast job can complete before we are listening.
- **The agent refuses verbs against its own unit.** Stopping the unit hosting
  the agent severs the very session the `CommandResult` would return on, and the
  machine goes dark until someone SSHes in. This is a correctness rule, not a
  policy judgment: the reply path cannot survive the command. Every other unit —
  `sshd`, `systemd-networkd`, anything — is the operator's call, consistent with
  "Cockpit, but centralized". Enforced **agent-side** so it holds no matter
  which client dispatched the verb.
- **The agent's own unit is discovered at runtime, not hard-coded.** On connect
  the agent asks the bus `GetUnitByPID(getpid())` and reads that unit's `Id`
  property. Nothing in this repo currently pins the agent's unit name — there is
  no `.service` file or deploy directory yet (PRD §5.1 leaves it to the
  provisioning template), so a hard-coded constant would be an assumption that
  silently stops matching the day the template names it something else. A guard
  that looks like it works but does not is worse than none. `argus-agent.service`
  remains only as a fallback for when `GetUnitByPID` fails (agent not running
  under systemd at all — a dev laptop, or a container), where the guard is
  moot anyway because there is no session-hosting unit to stop.
- **Agent collects by periodic poll** on the existing 15s sender tick, plus once
  right after `Hello`. Mirrors the Docker slice. Subscribing to per-unit
  `PropertiesChanged` would give instant updates at a large complexity cost and
  is a future optimization, not a V1 need.
- **Systemd state lives in-memory** as a latest snapshot per machine, exactly
  like the Docker cache: the PRD calls it the *current* unit list, the agent
  re-sends a full snapshot on every reconnect, and the control plane is a
  stateless single replica. **No new migration in this slice.**
- **Verbs execute in the existing spawned task** on the agent (`tokio::spawn`,
  the CLAUDE.md loss-tolerant fire-and-forget case) so a unit with a long
  `TimeoutStopSec` cannot block heartbeats or the inbound loop.

## Server

### `hub.rs`

Add a third snapshot map beside `docker`, with the same shape and the same
lifetime rules:

- `systemd: Mutex<HashMap<Uuid, Vec<Unit>>>`
- `set_systemd(machine_id, units)` / `get_systemd(machine_id) -> Vec<Unit>`
  (empty when nothing reported yet).
- `failed_unit_count(machine_id) -> usize` — units whose `active_state ==
  "failed"`. Counted in the hub rather than the handler so the fleet query stays
  a single cheap read under one lock.

No change to `conns`, `pending`, epochs, or `send_command` — unit verbs reuse
the command path the Docker slice built, unchanged.

### `grpc.rs`

`handle_agent_frame` gains one arm:

- `SystemdState(ss)` → `hub.set_systemd(machine_id, ss.units)` +
  `touch_last_seen`, symmetric with the existing `DockerState` arm.

`CommandResult` already routes generically by `command_id`, so unit verbs need
nothing there.

### `http.rs`

- `GET /api/machines/{id}/systemd` → `hub.get_systemd(id)` → JSON `{ units }`
  (empty list when the agent has not reported / has no system bus).
- `POST /api/machines/{id}/units/{unit}/{action}` (`action ∈
  start|stop|restart`) — the identical flow the container endpoint already runs:
  1. Validate `action → Verb`; `400` on unknown action.
  2. `400` on an empty unit name or one containing `/` (not a legal unit name,
     and a path-traversal shape we should never forward).
  3. `409` if the machine is not in `conns` (not reachable), audited as denied.
  4. Generate `command_id`; audit row `unit.<action>` with `target_ref`,
     `command_id`, result `dispatched`. Fails closed if the audit write fails.
  5. `hub.register_pending` + `hub.send_command`.
  6. `await` up to 10s: on `CommandResult` → `repo::update_command_result` and
     return `{command_id, ok, message}`; on timeout → `202 {command_id, status:
     "pending"}`.

**Refactor rather than copy:** `run_container_verb` currently hard-codes its
action→verb table and its audit action prefix. Generalize it into one
`run_verb(state, machine_id, verb, target, audit_action)` helper that both
endpoints call, with each endpoint owning only its own action→`Verb` mapping.
The two flows are otherwise byte-for-byte identical, and a copy would guarantee
they drift.

- `fleet` gains `failed_units: usize` per row, read from
  `hub.failed_unit_count(id)` alongside the existing DB query. It reads `0` for a
  machine that has never reported, and the UI only renders the chip when the
  count is `> 0`.
  - **Staleness, stated honestly:** snapshots are *not* evicted on disconnect
    (`Hub::unregister` only clears the connection registry), so for an offline
    machine this is the **last known** count, not a live one — a machine that
    goes offline with 3 failed units keeps reporting 3 until it comes back. The
    row pairs the count with the machine's own `status`, so an operator sees
    `offline` beside it, but nothing yet marks *when* the snapshot was taken.
    Evicting on disconnect (or stamping an `as_of` the UI can render, and
    disabling row actions for non-`online` machines) is deferred to a follow-up;
    the same staleness already applies to the Docker snapshot from slice 3.

### `repo.rs`

No change. `audit_command` and `update_command_result` were built generic over
`action` and `target_ref` in the Docker slice and carry unit verbs as-is.

Actor and `Command.issued_by` remain the `"anonymous"` placeholder until OIDC
lands, as in the current slices. Every verb is audited from the start, per
CLAUDE.md.

## Agent

### New module `systemd.rs`

A sibling to `docker.rs`, with the same defensive shape:

- `SystemdClient { inner: Option<Connection> }`, cheaply cloneable.
- `connect() -> SystemdClient` — best-effort system-bus connection; `None`
  (warned once) when there is no bus, so containers without systemd degrade to
  an empty unit list and verbs that fail with a clear message instead of
  panicking. Calls `Manager.Subscribe()` once on connect — systemd only emits
  job signals while at least one client is subscribed — and resolves
  `self_unit` (below) in the same pass.
- `list_units() -> Vec<proto::Unit>` — the two `ListUnitsByPatterns` calls,
  unioned and deduplicated by unit name (a failed `.service` matches both
  queries and must appear once), mapped from the D-Bus `(ssssssouso)` tuple
  (`name`, `description`, `load_state`, `active_state`, `sub_state`, `followed`,
  `object_path`, `job_id`, `job_type`, `job_object_path`) onto the proto's five
  fields. **Partial failure is not silent data loss:** if either query errors the
  whole call returns empty and logs, rather than reporting a partial list — a
  services-only list that dropped the failed-unit query would render as "nothing
  is wrong on this host", which is the one wrong answer this slice must never
  give.
- `run_verb(command_id, verb, target) -> CommandResult` — normalize the target
  (append `.service` when the name carries no unit suffix, as `systemctl` does),
  refuse it if it resolves to the agent's own unit, then open the `JobRemoved`
  stream, call `StartUnit` / `StopUnit` / `RestartUnit` with mode `replace`, and
  await the signal whose job path matches the returned one.
- `self_unit: Option<String>` — resolved once at connect via
  `GetUnitByPID(getpid())` → the unit's `Id` property, with
  `argus-agent.service` as the fallback when that call fails. The refusal check
  compares the normalized target against it.

Pure functions — the D-Bus tuple → proto mapping, target normalization, the
self-unit check, and job-result → `CommandResult` mapping — are factored out and
unit-tested without a bus, exactly as `docker.rs` does.

### `session.rs`

- Construct `SystemdClient::connect()` beside `DockerClient::connect()` and
  clone it into both the sender task and the inbound path.
- Sender task: after `Hello`, and on each 15s tick, send a `SystemdState` frame
  next to the existing `DockerState` one.
- Inbound `Command` arm: route on the verb — `CONTAINER_*` to `DockerClient`,
  `UNIT_*` to `SystemdClient` — inside the existing spawned task. An unknown
  verb returns a failed `CommandResult` rather than being dropped, so the
  browser's 10s wait resolves instead of timing out.

### `Cargo.toml`

Replace the `zbus -> slice 4 (systemd)` placeholder comment with the real
dependency: `zbus = { version = "5", default-features = false, features =
["tokio"] }`.

## Frontend

- `api.ts`: `Unit` type, `getSystemd(id)`, `unitAction(id, unit, action)` (URL-
  encoding the unit name), and `failed_units` on the fleet row type.
- `lib/queries.ts`: `useSystemd(id)` and `useUnitAction(id)` beside the existing
  container hooks, on the same TanStack Query polling cadence.
- `lib/status.ts`: `unitTone(active_state)` beside `containerTone` —
  `failed` → destructive, `active` → ok, everything else → muted.
- `components/UnitsCard.tsx`: the units table. Header reads `N units · M
  failed`. Above the table, a filter input (substring match on name and
  description) and a "failed only" checkbox. Rows sort failed → active →
  everything else, then by name within each group. Per-row actions mirror
  `ContainersCard`: Start when not active; Restart + Stop when active; `…` while
  that row's mutation is pending. All filtering and sorting is client-side over
  the cached snapshot — no new API parameters.
- `pages/MachineDetailPage.tsx`: a third tab, `units`, rendering `UnitsCard`.
- `pages/FleetPage.tsx`: a destructive-tone count chip on a machine's card when
  `failed_units > 0`.

## Build gate (first implementation task)

Mirroring the `bollard` gate in the Docker slice and the apalis-vs-pgmq ethos in
CLAUDE.md: **confirm `zbus` compiles for `x86_64-unknown-linux-musl` as a
`static-pie`, on the tokio reactor, without pulling in `async-io`/`async-std`,
openssl, or cmake.** zbus is pure Rust, but its default feature set selects the
smol reactor, and the agent must not carry two runtimes. Verify with `cargo tree
-p argus-agent -i async-io` coming back empty and the existing musl release
build succeeding. If it disappoints, fall back to a minimal hand-rolled D-Bus
client over the system socket, or to parsing `systemctl list-units
--output=json`. This precedes any agent systemd code.

## Testing

- **Server:**
  - `hub` unit tests: `set_systemd` / `get_systemd` round-trip and empty
    default; `failed_unit_count` counts only `active_state == "failed"` and
    returns `0` for an unknown machine.
  - `handle_agent_frame` seam test: a `SystemdState` frame populates the
    snapshot and touches `last_seen`.
  - `http` `oneshot` tests: `GET systemd` empty + populated (seeded via the
    hub); `POST` unit verb → `409` when offline (and audited denied), → `200
    {ok}` against a fake registered connection that echoes a `CommandResult`, →
    `202` on timeout, → `400` on an unknown action and on a malformed unit name;
    `fleet` reports `failed_units` from a seeded snapshot.
- **Agent:**
  - Pure unit tests for `systemd.rs`: D-Bus tuple → proto mapping, union +
    dedupe of the two pattern queries, target normalization (`nginx` →
    `nginx.service`, `foo.timer` unchanged), the self-unit refusal against an
    injected `self_unit` (matching both `argus-agent` and `argus-agent.service`,
    and *not* matching a lookalike such as `argus-agent-proxy.service`), and
    job-result → `CommandResult` mapping
    (`done` → ok; `failed` / `timeout` / `dependency` → not ok, message carries
    the result string).
  - Verb execution against a live bus gated `#[ignore]`, matching the repo's
    live-Postgres and live-daemon test convention.
- **Frontend:** the sort/filter predicate is a pure exported function so it is
  testable independently of the component.

## Out of scope / deferred

- D-Bus signal-driven (push) unit-state collection — periodic poll is enough for
  V1.
- Persisting unit state to Postgres — revisit only if a measured need appears.
- `enable` / `disable` / `mask` / `daemon-reload` verbs — not in the V1 proto.
- Journal log tailing for a unit — slice 5, which is where `journal:<unit>`
  sources land.
- OIDC actor identity on audit rows — lands with the browser-auth slice.
- A combined fleet health rollup spanning containers *and* units — the failed
  unit count is deliberately the only fleet-level signal added here.
