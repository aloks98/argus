# Docker Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent reports its Docker container state to the control plane, the machine-detail page lists those containers, and an operator can start/stop/restart a container from the browser — each verb audit-logged with its outcome surfaced back.

**Architecture:** The agent polls the local Docker daemon (bollard, unix socket only) and streams `DockerState` frames over the existing mTLS `Session`; the control plane caches the latest snapshot per machine in a new in-memory `Hub` shared between the gRPC and HTTP surfaces. Verb requests flow the other way: an HTTP `POST` looks up the agent's live outbound channel in the `Hub`, sends a `Command`, and waits (bounded) on a `command_id → oneshot` correlation for the agent's `CommandResult`. No proto change — the contract already exists.

**Tech Stack:** Rust (axum, tonic, sqlx/Postgres, bollard 0.19), React + TypeScript + `@e412/rnui-react`.

Design of record: `docs/superpowers/specs/2026-07-23-docker-slice-design.md`.

## Global Constraints

- **No proto change.** `DockerState`, `Container`, `Verb {CONTAINER_START, CONTAINER_STOP, CONTAINER_RESTART}`, `Command`, `CommandResult` already exist in `crates/proto/proto/argus.proto`.
- **Agent stays musl-static.** `bollard` must be added with `default-features = false, features = ["pipe"]` — the `pipe` feature is the unix-socket connector (hyperlocal); it pulls **no** TLS/openssl/cmake. Never enable `ssl`/`aws-lc-rs`/`http` unless the build gate forces it (and even bollard's TLS is ring, never openssl). The agent talks only to the **local** daemon socket.
- **Every verb is audit-logged from the start** (CLAUDE.md). A verb without an `audit_log` write is incomplete.
- **Docker state is in-memory only** — a live cache keyed by `machine_id`, re-sent by the agent on every reconnect. No new migration, no DB table.
- **sqlx is compile-time-checked** against the dev Postgres (`DATABASE_URL` wired via git-ignored `.cargo/config.toml` → `postgres://postgres:argus@localhost:5432/argus`). After adding any new `query!`, regenerate the committed offline cache with `cargo sqlx prepare --workspace` and commit `.sqlx/`.
- **UI is Rnui** (`@e412/rnui-react`), not Titanium.
- **Placeholder audit actor** is `"anonymous"` until OIDC lands (mirrors the intentionally-unauthenticated `/api/fleet`).
- **Crypto provider is ring everywhere.** Do not pull `aws-lc-rs`.

**Test prerequisite:** a dev Postgres must be running for `#[sqlx::test]` (it creates ephemeral per-test DBs):
```bash
docker run -d --name argus-pg -e POSTGRES_PASSWORD=argus -e POSTGRES_DB=argus -p 5432:5432 postgres:17
```

---

### Task 1: Server — `Hub` + gRPC session/frame wiring

The in-memory connection/state/correlation registry, plus the gRPC side that fills it: session register/unregister, inbound `DockerState` caching, and inbound `CommandResult` routing (+ audit update). The HTTP-only `Hub` methods are built and unit-tested here but carry a temporary `#[allow(dead_code)]` removed in Task 2.

**Files:**
- Create: `crates/server/src/hub.rs`
- Modify: `crates/server/src/main.rs` (add `mod hub;`, construct `Arc<Hub>`, pass to `AgentSvc::new`)
- Modify: `crates/server/src/grpc.rs` (`AgentSvc` holds `Arc<Hub>`; `session` registers/unregisters; `handle_agent_frame` gains `DockerState` + `CommandResult` arms and a `&Hub` param; update existing seam tests)
- Modify: `crates/server/src/repo.rs` (add `update_command_result`)

**Interfaces:**
- Produces:
  - `crate::hub::Hub` (`Arc`-shared) with:
    - `fn new() -> Hub`
    - `fn register(&self, machine_id: Uuid, tx: mpsc::Sender<Result<ServerFrame, Status>>) -> u64` (returns an epoch)
    - `fn unregister(&self, machine_id: Uuid, epoch: u64)` (epoch-guarded)
    - `fn set_docker(&self, machine_id: Uuid, containers: Vec<Container>)`
    - `fn get_docker(&self, machine_id: Uuid) -> Vec<Container>`
    - `async fn send_command(&self, machine_id: Uuid, command_id: String, verb: Verb, target: String, issued_by: String) -> Result<(), DispatchError>`
    - `fn register_pending(&self, command_id: String) -> oneshot::Receiver<CommandResult>`
    - `fn abandon_pending(&self, command_id: &str)`
    - `fn complete(&self, command_id: &str, result: CommandResult)`
    - `pub enum DispatchError { NotConnected }`
  - `repo::update_command_result(executor, command_id: Uuid, result: &str) -> anyhow::Result<()>`
  - `grpc::AgentSvc::new(ca: Arc<CertAuthority>, pool: PgPool, hub: Arc<Hub>)`

- [ ] **Step 1: Write `hub.rs` with the struct, methods, and unit tests**

Create `crates/server/src/hub.rs`:

```rust
//! In-memory session hub (Docker slice): the live agent-connection registry, the
//! latest Docker snapshot per machine, and the command_id -> waiter correlation
//! map for synchronous verb results. Shared as an `Arc<Hub>` between the gRPC
//! surface (which fills it from the Session stream) and the HTTP surface (which
//! reads snapshots and dispatches verbs). All state is in-memory and re-derived
//! on reconnect — consistent with the stateless single-replica control plane.

use argus_proto::v1::{server_frame, Command, CommandResult, Container, ServerFrame, Verb};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

/// One live agent connection: its outbound frame channel plus a per-connection
/// counter for the fresh non-zero `stream_id` each dispatched command gets. The
/// `epoch` distinguishes successive connections of the same machine so a
/// lingering old session's teardown can't evict a freshly-reconnected one.
struct ConnHandle {
    tx: mpsc::Sender<Result<ServerFrame, Status>>,
    next_stream_id: AtomicU64,
    epoch: u64,
}

/// Why a command couldn't be dispatched.
pub enum DispatchError {
    /// No live Session for that machine (offline / just disconnected).
    NotConnected,
}

#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<Uuid, ConnHandle>>,
    docker: Mutex<HashMap<Uuid, Vec<Container>>>,
    pending: Mutex<HashMap<String, oneshot::Sender<CommandResult>>>,
    epoch_counter: AtomicU64,
}

impl Hub {
    pub fn new() -> Hub {
        Hub::default()
    }

    /// Register a live connection, returning its epoch. A re-register for the
    /// same machine replaces the old handle (last writer wins).
    pub fn register(&self, machine_id: Uuid, tx: mpsc::Sender<Result<ServerFrame, Status>>) -> u64 {
        let epoch = self.epoch_counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.conns.lock().unwrap().insert(
            machine_id,
            ConnHandle {
                tx,
                next_stream_id: AtomicU64::new(1),
                epoch,
            },
        );
        epoch
    }

    /// Remove a connection only if it is still the one with this `epoch` — a
    /// stale disconnect must not evict a newer reconnection.
    pub fn unregister(&self, machine_id: Uuid, epoch: u64) {
        let mut conns = self.conns.lock().unwrap();
        if conns.get(&machine_id).map(|h| h.epoch) == Some(epoch) {
            conns.remove(&machine_id);
        }
    }

    /// Replace the cached container snapshot for a machine.
    pub fn set_docker(&self, machine_id: Uuid, containers: Vec<Container>) {
        self.docker.lock().unwrap().insert(machine_id, containers);
    }

    /// The latest cached snapshot for a machine (empty if none reported yet).
    #[allow(dead_code)] // wired up by the HTTP docker endpoint (Task 2)
    pub fn get_docker(&self, machine_id: Uuid) -> Vec<Container> {
        self.docker
            .lock()
            .unwrap()
            .get(&machine_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Send a verb down the machine's Session on a fresh non-zero stream_id.
    #[allow(dead_code)] // wired up by the HTTP verb endpoint (Task 2)
    pub async fn send_command(
        &self,
        machine_id: Uuid,
        command_id: String,
        verb: Verb,
        target: String,
        issued_by: String,
    ) -> Result<(), DispatchError> {
        // Extract the channel + stream_id under the lock, then release it before
        // the async send (never hold a std Mutex guard across an await).
        let (tx, stream_id) = {
            let conns = self.conns.lock().unwrap();
            let handle = conns.get(&machine_id).ok_or(DispatchError::NotConnected)?;
            let stream_id = handle.next_stream_id.fetch_add(1, Ordering::Relaxed);
            (handle.tx.clone(), stream_id)
        };
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::Command(Command {
                command_id,
                verb: verb as i32,
                target,
                issued_by,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Register interest in a command's result; the returned receiver resolves
    /// when a matching `CommandResult` frame arrives (or errors if abandoned).
    #[allow(dead_code)] // wired up by the HTTP verb endpoint (Task 2)
    pub fn register_pending(&self, command_id: String) -> oneshot::Receiver<CommandResult> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(command_id, tx);
        rx
    }

    /// Drop a pending waiter (e.g. on dispatch failure or client timeout).
    #[allow(dead_code)] // wired up by the HTTP verb endpoint (Task 2)
    pub fn abandon_pending(&self, command_id: &str) {
        self.pending.lock().unwrap().remove(command_id);
    }

    /// Deliver a result to any waiter for this command_id (no-op if none).
    pub fn complete(&self, command_id: &str, result: CommandResult) {
        if let Some(tx) = self.pending.lock().unwrap().remove(command_id) {
            let _ = tx.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, state: &str) -> Container {
        Container {
            id: id.into(),
            name: format!("name-{id}"),
            image: "img".into(),
            state: state.into(),
            status: "Up".into(),
            health: String::new(),
        }
    }

    #[test]
    fn set_then_get_docker_round_trips_and_defaults_empty() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        assert!(hub.get_docker(m).is_empty());
        hub.set_docker(m, vec![container("a", "running")]);
        let got = hub.get_docker(m);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a");
    }

    #[test]
    fn stale_unregister_does_not_evict_a_newer_connection() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx1, _rx1) = mpsc::channel(1);
        let epoch1 = hub.register(m, tx1);
        // Machine reconnects: a second register replaces the first.
        let (tx2, _rx2) = mpsc::channel(1);
        let _epoch2 = hub.register(m, tx2);
        // The OLD session's teardown must not remove the new connection.
        hub.unregister(m, epoch1);
        assert!(
            hub.conns.lock().unwrap().contains_key(&m),
            "newer connection must survive a stale unregister"
        );
    }

    #[tokio::test]
    async fn send_command_to_absent_machine_errors() {
        let hub = Hub::new();
        let res = hub
            .send_command(
                Uuid::new_v4(),
                "cmd-1".into(),
                Verb::ContainerStart,
                "c1".into(),
                "anonymous".into(),
            )
            .await;
        assert!(matches!(res, Err(DispatchError::NotConnected)));
    }

    #[tokio::test]
    async fn send_command_delivers_a_command_frame_on_a_nonzero_stream() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(4);
        hub.register(m, tx);
        hub.send_command(m, "cmd-9".into(), Verb::ContainerRestart, "web".into(), "anonymous".into())
            .await
            .expect("dispatch");
        let frame = rx.recv().await.unwrap().unwrap();
        assert_ne!(frame.stream_id, 0, "commands ride a non-zero sub-stream");
        match frame.payload {
            Some(server_frame::Payload::Command(c)) => {
                assert_eq!(c.command_id, "cmd-9");
                assert_eq!(c.verb, Verb::ContainerRestart as i32);
                assert_eq!(c.target, "web");
            }
            other => panic!("expected a Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_pending_then_complete_delivers_result() {
        let hub = Hub::new();
        let rx = hub.register_pending("cmd-2".into());
        hub.complete(
            "cmd-2",
            CommandResult {
                command_id: "cmd-2".into(),
                ok: true,
                exit_code: 0,
                message: "done".into(),
            },
        );
        let got = rx.await.expect("result delivered");
        assert!(got.ok);
    }

    #[tokio::test]
    async fn abandoned_pending_never_delivers() {
        let hub = Hub::new();
        let rx = hub.register_pending("cmd-3".into());
        hub.abandon_pending("cmd-3");
        hub.complete(
            "cmd-3",
            CommandResult {
                command_id: "cmd-3".into(),
                ok: true,
                exit_code: 0,
                message: String::new(),
            },
        );
        assert!(rx.await.is_err(), "sender was dropped; receiver must error");
    }
}
```

- [ ] **Step 2: Register the module in `main.rs`**

In `crates/server/src/main.rs`, add `mod hub;` to the module list (alongside `mod grpc;`):

```rust
mod embed;
mod grpc;
mod http;
mod hub;
mod identity;
```

- [ ] **Step 3: Run the Hub unit tests — expect PASS**

Run: `cargo test -p argus-server hub::`
Expected: all six `hub::tests::*` PASS. (No DB needed for these.)

- [ ] **Step 4: Add `repo::update_command_result` with a failing test**

In `crates/server/src/repo.rs`, add this function (place it after `audit`):

```rust
/// Update the audit row for a dispatched command with its final result
/// (`ok`/`error`), matched by the correlated `command_id`. A no-op if no row
/// carries that command_id.
pub async fn update_command_result(
    executor: impl sqlx::PgExecutor<'_>,
    command_id: Uuid,
    result: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE audit_log SET result = $2 WHERE command_id = $1",
        command_id,
        result,
    )
    .execute(executor)
    .await?;

    Ok(())
}
```

Add this test to `repo.rs`'s `#[cfg(test)] mod tests` (it uses the `#[sqlx::test]` + `seed_machine` idioms already in the file):

```rust
#[sqlx::test]
async fn update_command_result_sets_result_by_command_id(pool: PgPool) -> anyhow::Result<()> {
    let machine_id = seed_machine(&pool, "cmd-audit-host").await;
    let command_id = Uuid::new_v4();

    // A dispatched-but-not-yet-resolved verb audit row.
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
         VALUES ('anonymous', 'container.restart', $1, 'web', $2, 'dispatched')",
        machine_id,
        command_id,
    )
    .execute(&pool)
    .await?;

    update_command_result(&pool, command_id, "ok").await?;

    let row = sqlx::query!(
        "SELECT result FROM audit_log WHERE command_id = $1",
        command_id,
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.result.as_deref(), Some("ok"));

    Ok(())
}
```

- [ ] **Step 5: Run it — expect PASS**

Run: `cargo test -p argus-server repo::tests::update_command_result_sets_result_by_command_id`
Expected: PASS (dev Postgres must be running).

- [ ] **Step 6: Thread the `Hub` through `AgentSvc` and `handle_agent_frame`**

In `crates/server/src/grpc.rs`:

Add imports near the top:
```rust
use crate::hub::Hub;
use argus_proto::v1::{
    agent_frame, server_frame, AgentFrame, EnrollRequest, EnrollResponse, HelloAck, ServerFrame,
};
```
(That replaces the existing `use argus_proto::v1::{...}` line — keep whatever was already imported plus the ones shown.)

Change the struct + constructor:
```rust
pub struct AgentSvc {
    pub ca: Arc<CertAuthority>,
    pub pool: PgPool,
    pub hub: Arc<Hub>,
}

impl AgentSvc {
    pub fn new(ca: Arc<CertAuthority>, pool: PgPool, hub: Arc<Hub>) -> Self {
        Self { ca, pool, hub }
    }
}
```

In `session`, register on connect and unregister on disconnect. Replace the block that creates the channel and spawns the inbound task with:
```rust
let (tx, rx) = mpsc::channel::<Result<ServerFrame, Status>>(16);
let pool = self.pool.clone();
let hub = self.hub.clone();
let epoch = hub.register(machine_id, tx.clone());
let mut inbound = request.into_inner();

tokio::spawn(async move {
    while let Some(item) = inbound.next().await {
        match item {
            Ok(frame) => {
                if let Err(e) = handle_agent_frame(&pool, &hub, machine_id, frame, &tx).await {
                    tracing::warn!(error = %e, %machine_id, "session: error handling agent frame");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, %machine_id, "session: inbound stream error");
                break;
            }
        }
    }
    hub.unregister(machine_id, epoch);
    tracing::info!(%machine_id, "session: agent disconnected");
});
```

Change `handle_agent_frame`'s signature and add the two new arms:
```rust
async fn handle_agent_frame(
    pool: &PgPool,
    hub: &Hub,
    machine_id: Uuid,
    frame: AgentFrame,
    tx: &mpsc::Sender<Result<ServerFrame, Status>>,
) -> anyhow::Result<()> {
    match frame.payload {
        // ... existing Hello / Heartbeat / Metrics arms unchanged ...

        Some(agent_frame::Payload::DockerState(ds)) => {
            hub.set_docker(machine_id, ds.containers);
            repo::touch_last_seen(pool, machine_id).await?;
        }
        Some(agent_frame::Payload::CommandResult(cr)) => {
            let command_id = cr.command_id.clone();
            if let Ok(uuid) = Uuid::parse_str(&command_id) {
                repo::update_command_result(pool, uuid, if cr.ok { "ok" } else { "error" }).await?;
            }
            hub.complete(&command_id, cr);
        }
        _ => {
            // PTY / log frames are later slices; ignore for now.
        }
    }

    Ok(())
}
```

- [ ] **Step 7: Update `main.rs` to construct and pass the `Hub`**

In `crates/server/src/main.rs`, after the CA is built and before `AgentSvc::new`:
```rust
let hub = std::sync::Arc::new(hub::Hub::new());
let agent_svc = grpc::AgentSvc::new(ca, pool.clone(), hub.clone());
```
(`hub.clone()` here keeps a handle for Task 2's HTTP surface; for now the Hub is used by the gRPC side.)

- [ ] **Step 8: Fix the existing `grpc.rs` seam tests for the new signature**

Every existing call to `handle_agent_frame(&pool, machine_id, frame, &tx)` in `grpc.rs`'s test module must become `handle_agent_frame(&pool, &hub, machine_id, frame, &tx)` with a `let hub = crate::hub::Hub::new();` constructed in the test. The affected tests are:
- `handle_agent_frame_hello_then_heartbeat` (two calls)
- `handle_agent_frame_hello_does_not_overwrite_another_machine_by_self_reported_id` (one call)
- `handle_agent_frame_metrics_inserts_row_and_touches_last_seen` (one call)

For each, add `let hub = crate::hub::Hub::new();` before the first `handle_agent_frame` call and insert `&hub` as the second argument.

- [ ] **Step 9: Add the new gRPC seam tests (DockerState + CommandResult)**

Append to `grpc.rs`'s test module:

```rust
/// A `DockerState` frame must cache the reported containers in the hub, keyed
/// by the authenticated machine_id.
#[sqlx::test]
async fn handle_agent_frame_docker_state_caches_snapshot(pool: PgPool) -> anyhow::Result<()> {
    let machine_id = repo::upsert_machine(
        &pool,
        &AgentInfoRow {
            machine_id: "m-docker-1".to_string(),
            hostname: "docker-host".to_string(),
            os: None,
            kernel: None,
            arch: None,
            primary_ip: None,
            agent_version: None,
        },
    )
    .await?;

    let hub = crate::hub::Hub::new();
    let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

    handle_agent_frame(
        &pool,
        &hub,
        machine_id,
        AgentFrame {
            stream_id: 0,
            payload: Some(agent_frame::Payload::DockerState(argus_proto::v1::DockerState {
                containers: vec![argus_proto::v1::Container {
                    id: "abc123".into(),
                    name: "nginx".into(),
                    image: "nginx:latest".into(),
                    state: "running".into(),
                    status: "Up 2 minutes (healthy)".into(),
                    health: "healthy".into(),
                }],
            })),
        },
        &tx,
    )
    .await?;

    let cached = hub.get_docker(machine_id);
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].name, "nginx");
    assert_eq!(cached[0].health, "healthy");

    Ok(())
}

/// A `CommandResult` frame must (a) update the dispatched audit row to its final
/// result and (b) wake any pending waiter registered for that command_id.
#[sqlx::test]
async fn handle_agent_frame_command_result_updates_audit_and_wakes_waiter(
    pool: PgPool,
) -> anyhow::Result<()> {
    let machine_id = repo::upsert_machine(
        &pool,
        &AgentInfoRow {
            machine_id: "m-cmdres-1".to_string(),
            hostname: "cmd-host".to_string(),
            os: None,
            kernel: None,
            arch: None,
            primary_ip: None,
            agent_version: None,
        },
    )
    .await?;

    let command_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
         VALUES ('anonymous', 'container.stop', $1, 'web', $2, 'dispatched')",
        machine_id,
        command_id,
    )
    .execute(&pool)
    .await?;

    let hub = crate::hub::Hub::new();
    let waiter = hub.register_pending(command_id.to_string());
    let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

    handle_agent_frame(
        &pool,
        &hub,
        machine_id,
        AgentFrame {
            stream_id: 7,
            payload: Some(agent_frame::Payload::CommandResult(argus_proto::v1::CommandResult {
                command_id: command_id.to_string(),
                ok: true,
                exit_code: 0,
                message: "ok".into(),
            })),
        },
        &tx,
    )
    .await?;

    // (a) waiter woken with the result
    let delivered = waiter.await.expect("waiter must be woken");
    assert!(delivered.ok);
    // (b) audit row updated
    let row = sqlx::query!(
        "SELECT result FROM audit_log WHERE command_id = $1",
        command_id,
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.result.as_deref(), Some("ok"));

    Ok(())
}
```

- [ ] **Step 10: Build + run the full server test suite**

Run: `cargo test -p argus-server`
Expected: everything PASS, including the updated seam tests and the two new ones. Also run `cargo build -p argus-server` and confirm **no warnings** (the four `#[allow(dead_code)]` methods keep the HTTP-only surface quiet until Task 2).

- [ ] **Step 11: Refresh the sqlx offline cache**

Run: `cargo sqlx prepare --workspace` (from the repo root, dev Postgres up).
This regenerates `.sqlx/` for the new `update_command_result` query so `SQLX_OFFLINE=true`/CI builds keep working.

- [ ] **Step 12: Commit**

```bash
git add crates/server/src/hub.rs crates/server/src/main.rs crates/server/src/grpc.rs crates/server/src/repo.rs .sqlx
git commit -m "feat(server): session hub — connection registry, docker cache, command routing

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Server — HTTP docker endpoints (state + verbs)

The browser surface: `GET .../docker` reads the cached snapshot; `POST .../docker/{container}/{action}` dispatches a verb and waits (bounded) for the result. Consumes the `Hub` methods flagged `#[allow(dead_code)]` in Task 1 — **remove those four allows** as they become used.

**Files:**
- Modify: `crates/server/src/hub.rs` (delete the four `#[allow(dead_code)]` attributes)
- Modify: `crates/server/src/repo.rs` (add `audit_command`)
- Modify: `crates/server/src/http.rs` (`AppState { pool, hub }`; two routes; DTOs; handlers; tests)
- Modify: `crates/server/src/main.rs` (pass `hub` to `http::serve`)

**Interfaces:**
- Consumes (from Task 1): `Hub::{get_docker, send_command, register_pending, abandon_pending, complete}`, `DispatchError`, `repo::update_command_result`. Note the machine-scoped signatures from Task 1's hardening: `register_pending(command_id: String, machine_id: Uuid)` and `complete(command_id: &str, machine_id: Uuid, result: CommandResult)` — pass the path `id` as the machine_id.
- Produces:
  - `repo::audit_command(executor, actor: &str, action: &str, machine_id: Option<Uuid>, target_ref: &str, command_id: Uuid, result: &str) -> anyhow::Result<()>`
  - `http::serve(cfg: &Config, pool: PgPool, hub: Arc<Hub>)` (signature change)
  - Routes: `GET /api/machines/{id}/docker`, `POST /api/machines/{id}/docker/{container}/{action}`

- [ ] **Step 1: Remove the four `#[allow(dead_code)]` attributes in `hub.rs`**

Delete the four lines `#[allow(dead_code)] // wired up by the HTTP ... (Task 2)` above `get_docker`, `send_command`, `register_pending`, and `abandon_pending`. (They're about to be used, so the lint is satisfied without the allow.)

- [ ] **Step 2: Add `repo::audit_command` with a failing test**

In `crates/server/src/repo.rs`, after `audit`:

```rust
/// Audit a dispatched verb: like `audit`, but also records the `target_ref`
/// (container id / unit name) and the `command_id` correlating this row to the
/// gRPC `Command` and its eventual `CommandResult` (see `update_command_result`).
pub async fn audit_command(
    executor: impl sqlx::PgExecutor<'_>,
    actor: &str,
    action: &str,
    machine_id: Option<Uuid>,
    target_ref: &str,
    command_id: Uuid,
    result: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
         VALUES ($1, $2, $3, $4, $5, $6)",
        actor,
        action,
        machine_id,
        target_ref,
        command_id,
        result,
    )
    .execute(executor)
    .await?;

    Ok(())
}
```

Add to `repo.rs` tests:

```rust
#[sqlx::test]
async fn audit_command_records_target_and_command_id(pool: PgPool) -> anyhow::Result<()> {
    let machine_id = seed_machine(&pool, "audit-cmd-host").await;
    let command_id = Uuid::new_v4();

    audit_command(
        &pool,
        "anonymous",
        "container.start",
        Some(machine_id),
        "web",
        command_id,
        "dispatched",
    )
    .await?;

    let row = sqlx::query!(
        "SELECT actor, action, target_ref, command_id, result
         FROM audit_log WHERE command_id = $1",
        command_id,
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.actor, "anonymous");
    assert_eq!(row.action, "container.start");
    assert_eq!(row.target_ref.as_deref(), Some("web"));
    assert_eq!(row.command_id, Some(command_id));
    assert_eq!(row.result.as_deref(), Some("dispatched"));

    Ok(())
}
```

- [ ] **Step 3: Run it — expect PASS**

Run: `cargo test -p argus-server repo::tests::audit_command_records_target_and_command_id`
Expected: PASS.

- [ ] **Step 4: Extend `AppState`, add imports, DTOs, and routes in `http.rs`**

In `crates/server/src/http.rs`:

Add imports:
```rust
use crate::hub::{DispatchError, Hub};
use argus_proto::v1::Verb;
use axum::routing::post;
use std::sync::Arc;
use std::time::Duration;
```

Change `AppState` and `serve`:
```rust
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
```

Add the two routes to `router` (after the metrics route):
```rust
.route("/api/machines/{id}/docker", get(machine_docker))
.route(
    "/api/machines/{id}/docker/{container}/{action}",
    post(container_action),
)
```

Add the DTO + handler for reading state:
```rust
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
async fn machine_docker(State(state): State<AppState>, Path(id): Path<Uuid>) -> Json<Vec<ContainerDto>> {
    let containers = state.hub.get_docker(id);
    Json(containers.into_iter().map(ContainerDto::from).collect())
}
```

Add the verb handler (thin wrapper + testable core with an injectable timeout):
```rust
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
    // not-yet-inserted row, freezing it at "dispatched" forever. Scoped to `id`:
    // only that machine's session may resolve this command (Task 1's `complete`
    // enforces the machine_id match).
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
        tracing::error!(error = %e, "container verb: dispatched audit write failed");
    }

    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_command(id, cid.clone(), verb, container.to_string(), actor.to_string())
        .await
    {
        state.hub.abandon_pending(&cid);
        // The agent is offline: no CommandResult will ever arrive to resolve the
        // row, so flip it to the terminal "denied" state here -- the one case the
        // grpc CommandResult arm cannot cover (the command was never delivered).
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
        Ok(Err(_)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "result channel closed").into_response()
        }
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
```

- [ ] **Step 5: Update `main.rs` to pass the hub to `http::serve`**

In `crates/server/src/main.rs`, change the `http::serve` call inside `try_join!`:
```rust
tokio::try_join!(
    http::serve(&cfg, pool.clone(), hub.clone()),
    grpc::serve(&cfg, agent_svc, server_identity),
    jobs::run(pool.clone()),
    jobs::prune_metrics(pool.clone()),
)?;
```

- [ ] **Step 6: Add HTTP tests**

In `http.rs`'s `#[cfg(test)] mod tests`, add these. They construct `AppState` directly (not via `router`) where they call `run_container_verb`, and use `router(...)` + `oneshot` for the GET path. Add `use argus_proto::v1::{server_frame, CommandResult};` and `use tokio::sync::mpsc;` and `use crate::hub::Hub;` to the test module.

```rust
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
async fn verb_on_offline_agent_returns_409_and_audits_denied(pool: PgPool) -> anyhow::Result<()> {
    let machine_id: Uuid = sqlx::query!(
        "INSERT INTO machines (machine_id, hostname, status) VALUES ('verb-offline', 'h', 'offline') RETURNING id"
    )
    .fetch_one(&pool)
    .await?
    .id;

    let (state, _hub) = app_state_with_hub(pool.clone());
    let resp = run_container_verb(&state, machine_id, "web", "restart", Duration::from_millis(200)).await;
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

    let resp = run_container_verb(&state, machine_id, "web", "start", Duration::from_secs(5)).await;
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

    let resp = run_container_verb(&state, machine_id, "web", "stop", Duration::from_millis(150)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = to_bytes(resp.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["status"], "pending");

    Ok(())
}

#[sqlx::test]
async fn verb_with_unknown_action_returns_400(pool: PgPool) -> anyhow::Result<()> {
    let (state, _hub) = app_state_with_hub(pool);
    let resp = run_container_verb(&state, Uuid::new_v4(), "web", "obliterate", Duration::from_millis(100)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}
```

Also update the two **existing** http tests that build state as `AppState { pool }` — change them to `AppState { pool, hub: Arc::new(Hub::new()) }` (in `fleet_lists_machines_with_status` and `fleet_machine_and_metrics_endpoints`).

- [ ] **Step 7: Run the full server suite**

Run: `cargo test -p argus-server`
Expected: all PASS. Run `cargo build -p argus-server` — **no warnings** (the four allows are gone and every method is now used).

- [ ] **Step 8: Refresh the sqlx cache + commit**

```bash
cargo sqlx prepare --workspace
git add crates/server/src/hub.rs crates/server/src/http.rs crates/server/src/repo.rs crates/server/src/main.rs .sqlx
git commit -m "feat(server): docker state endpoint + container verb endpoint

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Agent — bollard collection + verb execution

Add bollard (musl-safe), a `docker` module that lists containers and runs verbs against the local daemon, and wire it into the Session: stream `DockerState` on connect + each tick, and execute inbound `Command`s in spawned tasks.

**Files:**
- Modify: `crates/agent/Cargo.toml` (add `bollard`)
- Create: `crates/agent/src/docker.rs`
- Modify: `crates/agent/src/main.rs` (`mod docker;`)
- Modify: `crates/agent/src/session.rs` (construct client; send `DockerState`; handle `Command`)

**Interfaces:**
- Produces:
  - `docker::DockerClient` (`Clone`) with `fn connect() -> DockerClient`, `async fn list_containers(&self) -> Vec<Container>`, `async fn run_verb(&self, command_id: String, verb: Verb, target: &str) -> CommandResult`.

- [ ] **Step 1: Build gate — add bollard and confirm the musl build stays openssl-free**

In `crates/agent/Cargo.toml`, under the "Per-slice deps" comment, add:
```toml
# Docker slice: local daemon over its unix socket only (never TCP/TLS), so the
# `pipe` feature (hyperlocal) is all we need — no openssl/cmake, ring-only.
bollard = { version = "0.19", default-features = false, features = ["pipe"] }
```

Verify it compiles for the release musl target (add the target first if needed):
```bash
rustup target add x86_64-unknown-linux-musl
cargo build -p argus-agent --target x86_64-unknown-linux-musl
```
Expected: builds clean. Then confirm no openssl crept in:
```bash
cargo tree -p argus-agent -i openssl-sys 2>&1 | head -1
```
Expected: `error: package ID specification ... did not match any packages` (i.e. openssl-sys is absent). If `pipe` alone fails to expose `connect_with_socket_defaults`, widen to `features = ["http", "pipe"]` (still openssl-free) and re-verify. Do not enable `ssl`/`aws-lc-rs`.

- [ ] **Step 2: Write `docker.rs` with pure-mapping unit tests first**

Create `crates/agent/src/docker.rs`:

```rust
//! Docker collection + verb execution (Docker slice). Talks ONLY to the local
//! daemon over its unix socket via bollard — never TCP/TLS, so the musl-static
//! build stays free of openssl (see Cargo.toml). Mapping is factored into pure
//! functions so it's unit-testable without a running daemon.

use argus_proto::v1::{CommandResult, Container, Verb};
use bollard::models::ContainerSummary;
use bollard::query_parameters::{
    ListContainersOptionsBuilder, RestartContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::Docker;
use std::time::Duration;

/// Cap any single daemon call so a hung dockerd can't stall the Session's
/// heartbeat/metrics sender.
const OP_TIMEOUT: Duration = Duration::from_secs(5);

/// A thin, cheaply-cloneable handle to the local Docker daemon. `inner` is
/// `None` on hosts without Docker (many LXC guests) — such a client reports an
/// empty container list and fails verbs with a clear message, never panicking.
#[derive(Clone)]
pub struct DockerClient {
    inner: Option<Docker>,
}

impl DockerClient {
    /// Best-effort connect to the local socket. Never fails.
    pub fn connect() -> DockerClient {
        match Docker::connect_with_socket_defaults() {
            Ok(d) => DockerClient { inner: Some(d) },
            Err(e) => {
                tracing::warn!(error = %e, "docker: no local daemon; container features disabled");
                DockerClient { inner: None }
            }
        }
    }

    /// All containers (running + stopped) mapped to proto `Container`. Empty on
    /// no-daemon or any listing error (logged) — the fleet view just shows none.
    pub async fn list_containers(&self) -> Vec<Container> {
        let Some(docker) = &self.inner else {
            return Vec::new();
        };
        let opts = ListContainersOptionsBuilder::new().all(true).build();
        match tokio::time::timeout(OP_TIMEOUT, docker.list_containers(Some(opts))).await {
            Ok(Ok(summaries)) => summaries.into_iter().map(summary_to_container).collect(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "docker: list_containers failed");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!("docker: list_containers timed out");
                Vec::new()
            }
        }
    }

    /// Run a container verb against `target` (id or name), producing a
    /// `CommandResult` correlated by `command_id`.
    pub async fn run_verb(&self, command_id: String, verb: Verb, target: &str) -> CommandResult {
        let Some(docker) = &self.inner else {
            return result(command_id, false, "docker daemon not available on this host");
        };
        let outcome = match verb {
            Verb::ContainerStart => {
                docker.start_container(target, None::<StartContainerOptions>).await
            }
            Verb::ContainerStop => {
                docker.stop_container(target, None::<StopContainerOptions>).await
            }
            Verb::ContainerRestart => {
                docker.restart_container(target, None::<RestartContainerOptions>).await
            }
            other => return result(command_id, false, &format!("unsupported verb {other:?}")),
        };
        match outcome {
            Ok(()) => result(command_id, true, "ok"),
            Err(e) => result(command_id, false, &e.to_string()),
        }
    }
}

fn result(command_id: String, ok: bool, message: &str) -> CommandResult {
    CommandResult {
        command_id,
        ok,
        exit_code: if ok { 0 } else { 1 },
        message: message.to_string(),
    }
}

/// Map a bollard container summary to the proto. Pure — unit-tested below.
fn summary_to_container(s: ContainerSummary) -> Container {
    let name = s
        .names
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_default();
    let status = s.status.unwrap_or_default();
    let health = parse_health(&status);
    Container {
        id: s.id.unwrap_or_default(),
        name,
        image: s.image.unwrap_or_default(),
        state: s.state.map(|st| st.to_string()).unwrap_or_default(),
        status,
        health,
    }
}

/// Extract docker's health hint from the ps-style status string, e.g.
/// "Up 2 minutes (healthy)" -> "healthy". "" when no health is present.
fn parse_health(status: &str) -> String {
    if status.contains("(healthy)") {
        "healthy".to_string()
    } else if status.contains("(unhealthy)") {
        "unhealthy".to_string()
    } else if status.contains("(health: starting)") {
        "starting".to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::ContainerSummaryStateEnum;

    #[test]
    fn parse_health_reads_the_status_parenthetical() {
        assert_eq!(parse_health("Up 2 minutes (healthy)"), "healthy");
        assert_eq!(parse_health("Up 5 seconds (health: starting)"), "starting");
        assert_eq!(parse_health("Up 3 hours (unhealthy)"), "unhealthy");
        assert_eq!(parse_health("Up 3 hours"), "");
        assert_eq!(parse_health("Exited (0) 2 days ago"), "");
    }

    #[test]
    fn summary_to_container_strips_leading_slash_and_maps_state() {
        let s = ContainerSummary {
            id: Some("abc123".into()),
            names: Some(vec!["/nginx".into()]),
            image: Some("nginx:latest".into()),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            status: Some("Up 2 minutes (healthy)".into()),
            ..Default::default()
        };
        let c = summary_to_container(s);
        assert_eq!(c.id, "abc123");
        assert_eq!(c.name, "nginx");
        assert_eq!(c.image, "nginx:latest");
        assert_eq!(c.state, "running");
        assert_eq!(c.health, "healthy");
    }

    #[test]
    fn summary_to_container_tolerates_all_missing_fields() {
        let c = summary_to_container(ContainerSummary::default());
        assert_eq!(c.id, "");
        assert_eq!(c.name, "");
        assert_eq!(c.state, "");
        assert_eq!(c.health, "");
    }
}
```

- [ ] **Step 3: Register the module and run the mapping tests**

In `crates/agent/src/main.rs`, add `mod docker;` to the module list:
```rust
mod config;
mod docker;
mod enroll;
```

Run: `cargo test -p argus-agent docker::`
Expected: the three `docker::tests::*` PASS (no daemon needed).

- [ ] **Step 4: Wire `DockerClient` into the session sender + inbound loop**

In `crates/agent/src/session.rs`:

Add imports (extend the existing `argus_proto` import line and add the docker use):
```rust
use crate::docker::DockerClient;
use argus_proto::v1::{agent_frame, server_frame, AgentFrame, DockerState, Heartbeat, Hello, Verb};
```

Construct the client once in `run` and pass it into `connect_and_serve`. In `run`, before the loop:
```rust
let docker = DockerClient::connect();
```
and change the call inside the loop:
```rust
let outcome = connect_and_serve(cfg, &identity, &docker).await;
```

Change `connect_and_serve`'s signature:
```rust
async fn connect_and_serve(cfg: &Config, identity: &Identity, docker: &DockerClient) -> Result<()> {
```

Give the sender task a docker handle and an initial + per-tick `DockerState`. Inside `connect_and_serve`, before spawning the sender, clone a handle for the inbound path:
```rust
let (tx, rx) = mpsc::channel::<AgentFrame>(16);
let inbound_tx = tx.clone();
let inbound_docker = docker.clone();
let sender_docker = docker.clone();
```

In the sender task (the `tokio::spawn(async move { ... })`), after the `Hello` send succeeds and before the ticker loop, send an initial snapshot:
```rust
// Initial Docker snapshot right after Hello so the panel populates promptly
// (the first ticker tick is a full interval away).
let containers = sender_docker.list_containers().await;
if tx
    .send(AgentFrame {
        stream_id: argus_common::CONTROL_STREAM_ID,
        payload: Some(agent_frame::Payload::DockerState(DockerState { containers })),
    })
    .await
    .is_err()
{
    return;
}
```

Inside the ticker loop, after the metrics send block, add a Docker send:
```rust
let containers = sender_docker.list_containers().await;
if tx
    .send(AgentFrame {
        stream_id: argus_common::CONTROL_STREAM_ID,
        payload: Some(agent_frame::Payload::DockerState(DockerState { containers })),
    })
    .await
    .is_err()
{
    tracing::debug!(agent_id = %sender_agent_id, "session: docker sender exiting, channel closed");
    return;
}
```
(Note: `sender_docker` is moved into the sender task; ensure the `let sender_docker = docker.clone();` above is captured by the `async move`.)

In the inbound drain loop (the `result = async { ... }` future), replace the `Some(Ok(_frame)) => { ... }` arm with `Command` handling:
```rust
Some(Ok(frame)) => {
    if let Some(server_frame::Payload::Command(cmd)) = frame.payload {
        // Verbs run in their own task (loss-tolerant fire-and-forget) so a
        // slow stop can't stall the inbound loop or heartbeats.
        let stream_id = frame.stream_id;
        let docker = inbound_docker.clone();
        let out = inbound_tx.clone();
        tokio::spawn(async move {
            let verb = Verb::try_from(cmd.verb).unwrap_or(Verb::Unspecified);
            let result = docker.run_verb(cmd.command_id.clone(), verb, &cmd.target).await;
            let _ = out
                .send(AgentFrame {
                    stream_id,
                    payload: Some(agent_frame::Payload::CommandResult(result)),
                })
                .await;
        });
    }
    // HelloAck / Ping / other ServerFrames remain no-ops for this slice.
}
```

- [ ] **Step 5: Build the agent (host + musl) and run its tests**

Run:
```bash
cargo build -p argus-agent
cargo test -p argus-agent
cargo build -p argus-agent --target x86_64-unknown-linux-musl
```
Expected: all build clean, tests PASS, **no warnings**.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/Cargo.toml crates/agent/src/docker.rs crates/agent/src/main.rs crates/agent/src/session.rs Cargo.lock
git commit -m "feat(agent): docker collection + container verb execution (bollard)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Frontend — containers panel + verb buttons

Add a Containers card to the machine-detail page: an Rnui table of containers with per-row start/stop/restart buttons, polled on the page's existing 10s loop.

**Files:**
- Modify: `frontend/src/api.ts` (`Container` type, `getDocker`, `containerAction`)
- Modify: `frontend/src/MachineDetailPage.tsx` (state, polling, `ContainersCard`)

**Interfaces:**
- Consumes: `GET /api/machines/{id}/docker`, `POST /api/machines/{id}/docker/{container}/{action}` from Task 2.

- [ ] **Step 1: Add the API client functions**

Append to `frontend/src/api.ts`:
```ts
export type Container = {
  id: string;
  name: string;
  image: string;
  state: string;
  status: string;
  health: string;
};

export async function getDocker(id: string): Promise<Container[]> {
  const r = await fetch(`/api/machines/${id}/docker`);
  if (!r.ok) throw new Error(`docker ${r.status}`);
  return r.json();
}

export type ContainerAction = "start" | "stop" | "restart";

export type VerbResult = {
  command_id: string;
  ok: boolean | null;
  message: string | null;
  status: string;
};

export async function containerAction(
  id: string,
  container: string,
  action: ContainerAction,
): Promise<VerbResult> {
  const r = await fetch(
    `/api/machines/${id}/docker/${encodeURIComponent(container)}/${action}`,
    { method: "POST" },
  );
  // 200 (completed) and 202 (pending) both carry a VerbResult body; 4xx/5xx
  // (e.g. 409 agent offline) are surfaced as errors.
  if (!r.ok) throw new Error(`action failed: ${r.status}`);
  return r.json();
}
```

- [ ] **Step 2: Add the Containers card to the detail page**

In `frontend/src/MachineDetailPage.tsx`:

Extend the Rnui import to add the table primitives, `Table` etc.:
```tsx
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  Button,
  ButtonGroup,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  EmptyState,
  LineChart,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
```

Extend the api import:
```tsx
import { containerAction, getDocker, getMachine, getMetrics } from "./api";
import type { Container, ContainerAction, MachineDetail, MetricPoint } from "./api";
```

Add a container state-badge helper near `statusVariant`:
```tsx
const CONTAINER_STATE_VARIANT: Record<string, StatusBadgeVariant> = {
  running: "success",
  restarting: "info",
  paused: "info",
  created: "outline",
  exited: "secondary",
  dead: "secondary",
};

function containerStateVariant(state: string): StatusBadgeVariant {
  return CONTAINER_STATE_VARIANT[state] ?? "outline";
}
```

Add the `ContainersCard` component (above `export default function MachineDetailPage`):
```tsx
function ContainersCard({
  machineId,
  containers,
  onChanged,
}: {
  machineId: string;
  containers: Container[];
  onChanged: () => void;
}) {
  // container ids with a verb currently in flight -> disables those rows'
  // buttons. A per-container set (not a single slot) so two concurrent actions
  // on different rows don't clear each other's busy flag.
  const [busy, setBusy] = useState<Set<string>>(new Set());
  const [actionError, setActionError] = useState<string | null>(null);

  async function run(container: Container, action: ContainerAction) {
    setBusy((prev) => new Set(prev).add(container.id));
    setActionError(null);
    try {
      await containerAction(machineId, container.id, action);
      onChanged();
    } catch (err) {
      setActionError(
        err instanceof Error ? err.message : `failed to ${action} ${container.name}`,
      );
    } finally {
      setBusy((prev) => {
        const next = new Set(prev);
        next.delete(container.id);
        return next;
      });
    }
  }

  return (
    <Card className="mt-6">
      <CardHeader>
        <CardTitle>Containers</CardTitle>
        <CardDescription>Docker containers on this host</CardDescription>
      </CardHeader>
      <CardContent>
        {actionError !== null && (
          <Alert variant="destructive" className="mb-4">
            <AlertTitle>Action failed</AlertTitle>
            <AlertDescription>{actionError}</AlertDescription>
          </Alert>
        )}
        {containers.length === 0 ? (
          <EmptyState
            title="No containers"
            description="This host reported no Docker containers (or has no Docker daemon)."
          />
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Image</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {containers.map((c) => {
                const running = c.state === "running";
                const rowBusy = busy.has(c.id);
                return (
                  <TableRow key={c.id}>
                    <TableCell className="font-medium">{c.name}</TableCell>
                    <TableCell className="text-gray-600">{c.image}</TableCell>
                    <TableCell>
                      <Badge variant={containerStateVariant(c.state)}>{c.state}</Badge>
                      {c.health !== "" && (
                        <Badge variant="outline" className="ml-1">
                          {c.health}
                        </Badge>
                      )}
                    </TableCell>
                    <TableCell className="text-gray-600">{c.status}</TableCell>
                    <TableCell className="text-right">
                      <ButtonGroup>
                        {running ? (
                          <>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() => void run(c, "restart")}
                            >
                              {rowBusy ? "…" : "Restart"}
                            </Button>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() => void run(c, "stop")}
                            >
                              {rowBusy ? "…" : "Stop"}
                            </Button>
                          </>
                        ) : (
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={rowBusy}
                            onClick={() => void run(c, "start")}
                          >
                            {rowBusy ? "…" : "Start"}
                          </Button>
                        )}
                      </ButtonGroup>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </CardContent>
    </Card>
  );
}
```

Wire it into `MachineDetailPage`: add container state, include `getDocker` in the poll, and render the card. Add state near the other `useState`s:
```tsx
const [containers, setContainers] = useState<Container[]>([]);
```

In the effect's `poll`, extend the `Promise.all` and set state:
```tsx
const [machineData, metricsData, dockerData] = await Promise.all([
  getMachine(id as string),
  getMetrics(id as string, range),
  getDocker(id as string),
]);
if (cancelled) return;
setMachine(machineData);
setMetrics(metricsData);
setContainers(dockerData);
```

Add a `refetchDocker` callback the card can call after a verb (so the UI reflects the new state without waiting for the next poll). Above the return, after `machine` is known non-null:
```tsx
const refetchDocker = () => {
  void getDocker(id).then(setContainers).catch(() => {});
};
```

Render the card just after the header `Card` (before the `<div className="mt-6 flex ...">` metrics controls):
```tsx
<ContainersCard
  machineId={id}
  containers={containers}
  onChanged={refetchDocker}
/>
```

- [ ] **Step 3: Typecheck + build the frontend**

Run:
```bash
npm --prefix frontend run build
```
Expected: `tsc` typecheck passes and Vite build succeeds (this is the project's frontend gate — there is no unit-test harness for the frontend).

- [ ] **Step 4: Commit**

```bash
git add frontend/src/api.ts frontend/src/MachineDetailPage.tsx
git commit -m "feat(frontend): container panel with start/stop/restart on machine detail

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: End-to-end verification + dev docs

Prove the slice works against a real Docker daemon, the way the Spine and Metrics slices were manually verified, and record it.

**Files:**
- Modify: `docs/DEV.md` (append a "Docker slice end-to-end verification" section)

- [ ] **Step 1: Full workspace build + test**

Run:
```bash
npm --prefix frontend run build
cargo build --workspace
cargo test --workspace
```
Expected: all green, no warnings.

- [ ] **Step 2: Run the stack and enroll an agent on a host with Docker**

Follow `docs/DEV.md` "Run the control plane" and "Enroll an agent" on a machine that has Docker running (so there are containers to see). Confirm the agent connects and the machine shows `online` on the fleet page.

- [ ] **Step 3: Verify container state end-to-end**

- `GET http://127.0.0.1:8080/api/machines/<id>/docker` returns the host's containers with `state`/`status`/`health` populated.
- The machine-detail page (`/machines/<id>`) shows the Containers card listing them, with an empty state on a host without Docker.

- [ ] **Step 4: Verify verbs end-to-end**

- Click **Stop** on a running container → within ~10s the button flips to **Start** on the next poll; `docker ps` on the host confirms it stopped.
- Click **Start** → it comes back `running`.
- Confirm audit rows:
  ```bash
  docker exec argus-pg psql -U postgres -d argus -c \
    "SELECT action, target_ref, result FROM audit_log WHERE action LIKE 'container.%' ORDER BY ts DESC LIMIT 5"
  ```
  Expect `container.stop` / `container.start` rows whose `result` transitioned to `ok`.
- Stop the agent, then POST a verb → the endpoint returns **409** and an audit row with `result = denied`.

- [ ] **Step 5: Record the verification in `docs/DEV.md` and commit**

Append a dated "Docker slice end-to-end verification" section to `docs/DEV.md` summarizing the observed results (mirroring the Spine/Metrics sections), then:
```bash
git add docs/DEV.md
git commit -m "docs: record Docker slice end-to-end verification

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** (against `docs/superpowers/specs/2026-07-23-docker-slice-design.md`):
- Proto unchanged — Global Constraints + all tasks reuse existing messages. ✓
- Hub (conns/docker/pending, epoch-guarded register/unregister, send_command, register_pending/abandon/complete, get/set_docker) — Task 1 Step 1. ✓
- gRPC DockerState + CommandResult arms, session register/unregister — Task 1 Steps 6, 9. ✓
- HTTP GET docker + POST verb (400/409/200/202), synchronous bounded wait — Task 2 Steps 4, 6. ✓
- Audit `audit_command` + `update_command_result`, `"anonymous"` actor — Task 1 Step 4, Task 2 Step 2. ✓
- Agent `docker.rs` (connect/list/run_verb, health/state mapping), session DockerState send + Command exec in spawned task — Task 3. ✓
- bollard build gate (musl, no openssl, `pipe` feature) — Task 3 Step 1. ✓
- Frontend container panel + verbs — Task 4. ✓
- Testing: hub unit, seam, http oneshot, docker mapping, ignore-gated verb exec (covered by the manual E2E in Task 5 rather than an `#[ignore]` test, since verb execution needs a live daemon) — Tasks 1–5. ✓
- Reachability via live registry, offline via existing sweeper — Task 2 (409 from `DispatchError::NotConnected`), unchanged sweeper. ✓

**Deviations from the spec (intentional):**
- The design mentioned `DockerSnapshot { containers, updated_at }`; the plan stores `Vec<Container>` directly and drops `updated_at` (YAGNI — the page polls; staleness isn't surfaced). If a staleness indicator is wanted later it's an additive change.
- Verb execution is verified live in Task 5 rather than via an `#[ignore]` unit test; a daemon-dependent unit test would add no coverage the manual E2E doesn't.

**Placeholder scan:** no TBD/TODO/"handle edge cases"/"similar to Task N" — every step has concrete code or exact commands. ✓

**Type consistency:** `send_command(machine_id, command_id, verb, target, issued_by)` matches its call in Task 2; `run_verb(command_id, verb, target)` matches its call in Task 3; `audit_command(...)` / `update_command_result(...)` signatures match their producers and callers; `ContainerDto`/`VerbResult` fields match the frontend `Container`/`VerbResult` types; `handle_agent_frame(&pool, &hub, machine_id, frame, &tx)` is consistent across the wiring and all tests. ✓
