# Docker slice — design

**Date:** 2026-07-23
**Build order:** slice 3 of 6 (PRD §8) — "Docker state + container verbs (`bollard`)".
**Depends on:** Spine (#1) + Metrics (#2), both merged.

## Goal

End-to-end, independently testable: an agent reports its Docker container state
to the control plane, the machine-detail page lists those containers, and an
operator can start / stop / restart a container from the browser — each verb
audit-logged and its outcome surfaced back to the user.

## Scope

- **In:** container state display + `start` / `stop` / `restart` verbs.
- **Out:** Docker *log* tailing (slice 5), terminal (slice 6). Container
  pause/remove/create and image ops are not in the V1 proto and are not added.

## Proto

**No proto change.** `DockerState`, `Container`, `Verb {CONTAINER_START,
CONTAINER_STOP, CONTAINER_RESTART}`, `Command`, and `CommandResult` already
exist in `crates/proto/proto/argus.proto`. This slice fills in behavior around
the existing contract.

## Decisions

- **Verb feedback = synchronous bounded wait.** The POST endpoint sends the
  `Command`, then waits up to ~10s for the agent's `CommandResult` via a
  `command_id → oneshot` correlation map; returns `{ok, message}` directly, or
  `202 {command_id, pending}` on timeout. Chosen for immediate feedback
  ("restarted" / "no such container") over async fire-and-forget.
- **Docker state lives in-memory** as a latest snapshot per machine, not a DB
  table. The PRD calls it the *current* container list, there is no migration
  for it, and the agent re-sends a full snapshot on every reconnect — it is a
  live cache, not durable state, consistent with the stateless single-replica
  pod design. No new migration in this slice.
- **Agent collects by periodic poll** (bollard `list_containers` on the existing
  15s sender tick + once right after `Hello`). Graceful when there is no Docker
  daemon (many LXC guests). Docker's event stream is a future optimization.
- **Verbs execute in a spawned task** on the agent (`tokio::spawn` — the literal
  CLAUDE.md example of loss-tolerant fire-and-forget) so a slow `stop` cannot
  block heartbeats or the inbound loop.
- **Reachability is checked against the live connection registry**, not the DB
  `status` column (which lags by up to the 45s offline-sweep window).
- **Disconnect** keeps relying on the existing offline sweeper for `status`;
  registry `unregister` only tears down the command path.

## Server

### New module `hub.rs` — the shared in-memory `Hub`

An `Arc<Hub>` constructed in `main.rs` and handed to **both** `grpc::AgentSvc`
and `http::AppState` (same process, joined via `try_join!`). Three maps behind
mutexes:

- `conns: HashMap<Uuid, ConnHandle>` — `machine_id → { tx:
  mpsc::Sender<Result<ServerFrame, Status>>, next_stream_id: AtomicU64, epoch }`.
  The server→agent command path.
- `docker: HashMap<Uuid, DockerSnapshot>` — `machine_id → { containers:
  Vec<Container>, updated_at }`. The live cache.
- `pending: HashMap<String, oneshot::Sender<CommandResult>>` — `command_id →
  waiter`. The synchronous-wait correlation map.

Methods:

- `register(machine_id, tx) -> epoch` / `unregister(machine_id, epoch)` —
  epoch-guarded so a lingering old session's disconnect cannot evict a
  freshly-reconnected one.
- `send_command(machine_id, command_id, verb, target, issued_by) -> Result<()>`
  — the handler generates `command_id` (so it can audit before dispatch); this
  assigns a fresh non-zero `stream_id`, sends `ServerFrame::Command`, and errors
  if the agent is not connected.
- `register_pending(command_id) -> oneshot::Receiver` / `complete(command_id,
  result)`.
- `set_docker(machine_id, containers)` / `get_docker(machine_id)`.

### gRPC wiring (`grpc.rs`)

- `session()`: after auth, `hub.register(machine_id, tx.clone())`; on loop exit
  `hub.unregister(machine_id, epoch)`.
- `handle_agent_frame` (now takes `&hub`) gains two arms:
  - `DockerState(ds)` → `hub.set_docker(machine_id, ds.containers)` +
    `touch_last_seen`.
  - `CommandResult(cr)` → `hub.complete(cr.command_id, cr)` **and**
    `repo::update_command_result(cr.command_id, ok ? "ok" : "error")`.

### HTTP surface (`http.rs`)

- `AppState { pool, hub }`.
- `GET /api/machines/{id}/docker` → `hub.get_docker(id)` → JSON `{ containers,
  updated_at }` (empty list when the agent has not reported / has no daemon).
- `POST /api/machines/{id}/docker/{container}/{action}` (`action ∈
  start|stop|restart`):
  1. Validate `action → Verb`; `400` on unknown action.
  2. `409` if the agent is not in `conns` (not reachable).
  3. Generate `command_id`; audit row `container.<action>` with `target_ref`,
     `command_id`, result `dispatched`.
  4. `hub.register_pending(command_id)`; `hub.send_command(machine_id,
     command_id, verb, target, issued_by)`.
  5. `await` up to 10s: on `CommandResult` → `repo::update_command_result` and
     return `{command_id, ok, message}`; on timeout → `202 {command_id, status:
     "pending"}`.

### Audit (`repo.rs`)

- `audit_command(actor, action, machine_id, target_ref, command_id, result)` —
  the existing `audit()` carries neither `target_ref` nor `command_id`.
- `update_command_result(command_id, result)`.
- Actor and `Command.issued_by` use a placeholder (`"anonymous"`) until OIDC
  lands, mirroring how `/api/fleet` is intentionally unauthenticated in the
  current slices. Every verb is audited from the start, per CLAUDE.md.

## Agent

### New module `docker.rs`

Wraps bollard:

- `connect() -> Option<Docker>` — best-effort local unix socket; `None` (logged
  once) if no daemon.
- `list_containers() -> Vec<proto::Container>` — map bollard's container summary
  to the proto: `id`, `name` (leading `/` stripped), `image`, `state`, `status`,
  `health` (from the health field; `""` when none).
- `run_verb(verb, target) -> CommandResult` — dispatch to bollard
  `start_container` / `stop_container` / `restart_container`; map success/error
  and any exit info into `CommandResult`.

Pure mapping functions (summary→proto, health/state mapping) are factored out so
they are unit-testable without a daemon.

### `session.rs`

- Clone `tx` for the inbound path (currently moved wholly into the sender task).
- Sender task: after `Hello`, and on each 15s tick, also poll Docker and send a
  `DockerState` frame (empty / skipped when there is no daemon).
- Inbound loop: on `Command`, **spawn a task** that runs `docker::run_verb` and
  sends `CommandResult` back on the command's `stream_id` via the cloned `tx`.

### `Cargo.toml`

Add `bollard` under the existing "per-slice deps" comment.

## Frontend

- `api.ts`: `Container` type, `getDocker(id)`, `containerAction(id, container,
  action)`.
- `MachineDetailPage.tsx`: a **Containers** Card above the metrics — an rnui
  table (name / image / state badge / health / status) with per-row action
  buttons (Start when stopped; Stop + Restart when running), optimistic "…"
  state on click, refetch on response, empty state when none. Polls on the
  existing 10s loop. (Mind the rnui theming gotcha noted in project memory.)

## Build gate (first implementation task)

Mirroring the apalis-vs-pgmq ethos in CLAUDE.md: **confirm `bollard` compiles
for `x86_64-unknown-linux-musl` without dragging in openssl / cmake.** Pick a
feature set that is unix-socket-only, no TLS (the agent only ever talks to the
*local* daemon). If it disappoints, fall back to a minimal hand-rolled socket
client. This precedes any agent Docker code.

## Testing

- **Server:**
  - `hub` unit tests: epoch-guarded register/unregister; `send_command` to an
    absent agent errors; pending `complete` vs timeout.
  - `handle_agent_frame` seam tests (extending the existing pattern): a
    `DockerState` frame updates the snapshot; a `CommandResult` frame completes
    the pending waiter and updates the audit row.
  - `http` `oneshot` tests: `GET docker` empty + populated (seed via the hub);
    `POST` verb → `409` when offline, → `200 {ok}` with a fake registered
    connection that echoes a `CommandResult`, → `202` on timeout.
- **Agent:**
  - Pure mapping unit tests for `docker.rs` (summary→proto, name strip, health
    mapping).
  - Verb execution gated `#[ignore]` (needs a live daemon, like the repo's
    live-Postgres tests).

## Out of scope / deferred

- Docker event-stream (push) collection — periodic poll is enough for V1.
- Persisting container state to Postgres — revisit only if a measured need
  appears.
- OIDC actor identity on audit rows — lands with the browser-auth slice.
- Fleet-page container summary badges — YAGNI for this slice.
