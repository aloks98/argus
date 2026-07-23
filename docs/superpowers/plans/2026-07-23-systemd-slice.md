# Systemd Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent reports its systemd unit state over the existing Session stream; the machine-detail page lists units failures-first and can start/stop/restart them; the fleet page shows which machines have failed units — every verb audit-logged.

**Architecture:** Slice 4 of 6 (PRD §8), built exactly on the rails the Docker slice laid: the agent polls on the existing 15s sender tick and emits a `SystemdState` frame; the server caches it in the in-memory `Hub` (no migration) and dispatches verbs through the existing `command_id → oneshot` correlation map; the browser reads a cached snapshot and POSTs verbs. The only place systemd genuinely differs from Docker is verb semantics — `StartUnit` returns when a job is *enqueued*, so the agent correlates systemd's `JobRemoved` signal to report the real outcome.

**Tech Stack:** Rust (tonic/axum/sqlx), `zbus` 5 on the agent's D-Bus system bus, React + TanStack Query + `@e412/rnui-react` on the frontend.

**Design of record:** `docs/superpowers/specs/2026-07-23-systemd-slice-design.md`

## Global Constraints

- **Proto is frozen for this slice.** `SystemdState`, `Unit`, and `Verb {UNIT_START=4, UNIT_STOP=5, UNIT_RESTART=6}` already exist in `crates/proto/proto/argus.proto`. Do not edit the proto.
- **No migration.** Unit state is an in-memory `Hub` cache, re-derived on reconnect. Do not add a table.
- **Every verb goes through the audit log** (CLAUDE.md). A verb path without an `audit_log` write is incomplete, and the dispatched-audit write **fails closed** — if it fails, do not dispatch.
- **The agent must keep building static for musl.** `zbus` is pinned `default-features = false, features = ["tokio"]`; never enable its default features (they pull the async-io/smol reactor). Verify with `cargo build -p argus-agent --release --target x86_64-unknown-linux-musl` (needs `CC_x86_64_unknown_linux_musl=musl-gcc`).
- **`ring` only.** No `aws-lc-rs`, no openssl, no cmake anywhere in the tree.
- **Reported unit set** = loaded `*.service` units **plus any unit in state `failed` regardless of type**, unioned and deduplicated by name.
- **Rust tests need Postgres.** `.cargo/config.toml` already exports `DATABASE_URL`; the dev database is the `argus-pg` container. `#[sqlx::test]` provisions a fresh schema per test.
- **Frontend has no test runner** (no vitest, no `test` script in `frontend/package.json`). Frontend verification is `npm --prefix frontend run typecheck` + `run build` + manual E2E. Pure helpers are still written as exported functions so a runner can pick them up later — do **not** add a test runner in this slice.
- **Never hold a `std::sync::Mutex` guard across an `.await`** — the existing `Hub` code extracts what it needs and drops the guard first. Follow that.

### Verified zbus 5 API (compiled during planning — use exactly these shapes)

`zbus 5.18`, `zvariant 5.13`. This probe compiled clean and built static-pie for musl:

```rust
use zbus::proxy;
use zvariant::OwnedObjectPath;

#[derive(Debug, Clone, serde::Deserialize, zvariant::Type)]
pub struct UnitRecord {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub followed: String,
    pub object_path: OwnedObjectPath,
    pub job_id: u32,
    pub job_type: String,
    pub job_object_path: OwnedObjectPath,
}

#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Manager {
    fn subscribe(&self) -> zbus::Result<()>;
    fn list_units_by_patterns(&self, states: Vec<String>, patterns: Vec<String>)
        -> zbus::Result<Vec<UnitRecord>>;
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn get_unit_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    fn job_removed(&self, id: u32, job: OwnedObjectPath, unit: String, result: String)
        -> zbus::Result<()>;
}

#[proxy(interface = "org.freedesktop.systemd1.Unit", default_service = "org.freedesktop.systemd1")]
pub trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}
```

Facts the compiler confirmed, which the code below depends on:
- The macro generates `ManagerProxy` / `UnitProxy` from trait `Manager` / `Unit`.
- `ManagerProxy::new(&conn)` works because `default_path` is set; `UnitProxy` has no default path, so it needs `UnitProxy::builder(&conn).path(p)?.build().await?`.
- `mgr.receive_job_removed().await?` returns a stream; `sig.args()?` yields accessors `args.job() -> &OwnedObjectPath` and `args.result() -> &str`. Requires `use futures_util::StreamExt;`.
- `std::process::id()` returns `u32`, which is what `get_unit_by_pid` takes.

---

### Task 1: Build gate — zbus compiles static for musl

**Status: ALREADY COMPLETE** — done during planning and committed as `e5cd283`. Verify it still holds, then move on.

**Files:**
- Modify: `crates/agent/Cargo.toml:36-51`

- [ ] **Step 1: Confirm the reactor constraint holds**

Run:
```bash
cargo tree -p argus-agent -i async-io; cargo tree -p argus-agent -i async-std
```
Expected: both print `error: package ID specification ... did not match any packages`. Anything else means zbus's default features leaked back in — stop and fix `crates/agent/Cargo.toml` before continuing.

- [ ] **Step 2: Confirm the static musl build**

Run:
```bash
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `... ELF 64-bit LSB pie executable, x86-64, ... static-pie linked, ... stripped`

No commit — this task's commit already exists.

---

### Task 2: Agent `systemd.rs` — pure mapping and policy functions

Everything in this task is testable without a D-Bus connection. The live-bus wiring is Task 3.

**Files:**
- Create: `crates/agent/src/systemd.rs`
- Modify: `crates/agent/src/main.rs:16` (add `mod systemd;`)

**Interfaces:**
- Consumes: nothing (first task with code).
- Produces, for Tasks 3 and 4:
  - `pub struct UnitRecord` (fields exactly as in Global Constraints above)
  - `fn record_to_unit(r: UnitRecord) -> argus_proto::v1::Unit`
  - `fn merge_units(services: Vec<UnitRecord>, failed: Vec<UnitRecord>) -> Vec<argus_proto::v1::Unit>`
  - `fn normalize_target(target: &str) -> String`
  - `fn is_self_unit(target: &str, self_unit: Option<&str>) -> bool`
  - `fn job_result_to_command_result(command_id: String, result: &str) -> argus_proto::v1::CommandResult`
  - `fn result(command_id: String, ok: bool, message: &str) -> argus_proto::v1::CommandResult`

- [ ] **Step 1: Write the failing tests**

Create `crates/agent/src/systemd.rs` containing ONLY the module doc, imports, the `UnitRecord` struct, and this test module (no implementations yet):

```rust
//! Systemd collection + unit verb execution (systemd slice). Talks to the local
//! D-Bus **system bus** via zbus — no network, no TLS, so the musl-static build
//! stays clean. Mapping and policy are factored into pure functions so they are
//! unit-testable without a running bus (mirrors `docker.rs`).

use argus_proto::v1::{CommandResult, Unit as ProtoUnit};
use zvariant::OwnedObjectPath;

/// One row of `ListUnitsByPatterns` — systemd's `(ssssssouso)` unit struct.
#[derive(Debug, Clone, serde::Deserialize, zvariant::Type)]
pub struct UnitRecord {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub followed: String,
    pub object_path: OwnedObjectPath,
    pub job_id: u32,
    pub job_type: String,
    pub job_object_path: OwnedObjectPath,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, active: &str) -> UnitRecord {
        let path = OwnedObjectPath::try_from("/").expect("root object path");
        UnitRecord {
            name: name.to_string(),
            description: format!("desc for {name}"),
            load_state: "loaded".to_string(),
            active_state: active.to_string(),
            sub_state: if active == "active" { "running" } else { "dead" }.to_string(),
            followed: String::new(),
            object_path: path.clone(),
            job_id: 0,
            job_type: String::new(),
            job_object_path: path,
        }
    }

    #[test]
    fn record_to_unit_maps_the_five_proto_fields() {
        let u = record_to_unit(record("nginx.service", "active"));
        assert_eq!(u.name, "nginx.service");
        assert_eq!(u.load_state, "loaded");
        assert_eq!(u.active_state, "active");
        assert_eq!(u.sub_state, "running");
        assert_eq!(u.description, "desc for nginx.service");
    }

    #[test]
    fn merge_units_deduplicates_a_failed_service_present_in_both_queries() {
        // A failed *.service matches BOTH ListUnitsByPatterns calls; it must
        // appear exactly once.
        let services = vec![record("nginx.service", "failed"), record("cron.service", "active")];
        let failed = vec![record("nginx.service", "failed"), record("data.mount", "failed")];

        let merged = merge_units(services, failed);

        let names: Vec<&str> = merged.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == "nginx.service").count(), 1);
        assert_eq!(merged.len(), 3, "nginx + cron + the failed .mount");
        assert!(names.contains(&"data.mount"), "failed non-service units must survive");
    }

    #[test]
    fn normalize_target_appends_service_only_when_there_is_no_unit_suffix() {
        assert_eq!(normalize_target("nginx"), "nginx.service");
        assert_eq!(normalize_target("nginx.service"), "nginx.service");
        assert_eq!(normalize_target("data.mount"), "data.mount");
        assert_eq!(normalize_target("backup.timer"), "backup.timer");
        assert_eq!(normalize_target("getty@tty1"), "getty@tty1.service");
    }

    #[test]
    fn is_self_unit_matches_both_forms_but_not_a_lookalike() {
        let me = Some("argus-agent.service");
        assert!(is_self_unit("argus-agent.service", me));
        assert!(is_self_unit("argus-agent", me), "bare name normalizes to .service");
        assert!(!is_self_unit("argus-agent-proxy.service", me), "prefix match must not count");
        assert!(!is_self_unit("nginx.service", me));
    }

    #[test]
    fn is_self_unit_is_false_when_the_self_unit_is_unknown() {
        // Not running under systemd: there is no session-hosting unit to protect.
        assert!(!is_self_unit("argus-agent.service", None));
    }

    #[test]
    fn job_result_done_is_ok_everything_else_is_not() {
        let ok = job_result_to_command_result("c1".into(), "done");
        assert!(ok.ok);
        assert_eq!(ok.exit_code, 0);

        for bad in ["failed", "timeout", "canceled", "dependency", "skipped"] {
            let r = job_result_to_command_result("c1".into(), bad);
            assert!(!r.ok, "job result {bad} must not be ok");
            assert!(r.message.contains(bad), "message must carry systemd's own result string");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

First add the module declaration. In `crates/agent/src/main.rs`, change line 16 from:

```rust
mod session;
```

to:

```rust
mod session;
mod systemd;
```

Run: `cargo test -p argus-agent systemd`
Expected: FAIL — `cannot find function 'record_to_unit' in this scope` and similar for each helper.

- [ ] **Step 3: Write the implementations**

Insert these above the `#[cfg(test)]` block in `crates/agent/src/systemd.rs`:

```rust
/// Unit-name suffixes systemd recognises. A target carrying none of these is a
/// bare service name, and gets `.service` appended — same rule `systemctl` uses.
const UNIT_SUFFIXES: &[&str] = &[
    ".service", ".socket", ".device", ".mount", ".automount", ".swap", ".target",
    ".path", ".timer", ".slice", ".scope",
];

/// Map a systemd `ListUnitsByPatterns` row onto the proto's five fields. Pure.
fn record_to_unit(r: UnitRecord) -> ProtoUnit {
    ProtoUnit {
        name: r.name,
        load_state: r.load_state,
        active_state: r.active_state,
        sub_state: r.sub_state,
        description: r.description,
    }
}

/// Union the two pattern queries, deduplicated by unit name. A failed
/// `*.service` matches both and must appear exactly once. Services come first
/// so the ordering is stable regardless of what the failed query returns.
fn merge_units(services: Vec<UnitRecord>, failed: Vec<UnitRecord>) -> Vec<ProtoUnit> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(services.len() + failed.len());
    for r in services.into_iter().chain(failed.into_iter()) {
        if seen.insert(r.name.clone()) {
            out.push(record_to_unit(r));
        }
    }
    out
}

/// Append `.service` unless the name already carries a unit suffix.
fn normalize_target(target: &str) -> String {
    if UNIT_SUFFIXES.iter().any(|s| target.ends_with(s)) {
        target.to_string()
    } else {
        format!("{target}.service")
    }
}

/// Whether `target` names the unit hosting this agent. Compared after
/// normalization so `argus-agent` and `argus-agent.service` both match, and by
/// full equality so `argus-agent-proxy.service` does not.
fn is_self_unit(target: &str, self_unit: Option<&str>) -> bool {
    match self_unit {
        Some(me) => normalize_target(target) == normalize_target(me),
        None => false,
    }
}

/// Build a `CommandResult` from systemd's own job-result string. Only `done`
/// means the job actually succeeded.
fn job_result_to_command_result(command_id: String, job_result: &str) -> CommandResult {
    if job_result == "done" {
        result(command_id, true, "done")
    } else {
        result(command_id, false, &format!("systemd job result: {job_result}"))
    }
}

/// Shared `CommandResult` constructor, mirroring `docker.rs`.
fn result(command_id: String, ok: bool, message: &str) -> CommandResult {
    CommandResult {
        command_id,
        ok,
        exit_code: if ok { 0 } else { 1 },
        message: message.to_string(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-agent systemd`
Expected: PASS — 6 tests.

Then run: `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: clean. (`UnitRecord`'s unread fields may warn as dead code; that resolves in Task 3 when the bus code reads them. If clippy fails only on that, proceed — Step 4 of Task 3 re-runs it.)

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/systemd.rs crates/agent/src/main.rs
git commit -m "feat(agent): pure systemd mapping and self-unit policy

The D-Bus unit record -> proto mapping, the two-query union/dedupe, target
normalization, the self-unit refusal check, and systemd's job-result
semantics ('done' alone means success), all as pure functions with tests
that need no running bus."
```

---

### Task 3: Agent `systemd.rs` — live bus client

**Files:**
- Modify: `crates/agent/src/systemd.rs`

**Interfaces:**
- Consumes from Task 2: `UnitRecord`, `record_to_unit`, `merge_units`, `normalize_target`, `is_self_unit`, `job_result_to_command_result`, `result`.
- Produces, for Task 4:
  - `pub struct SystemdClient` — `#[derive(Clone)]`
  - `pub async fn SystemdClient::connect() -> SystemdClient` (never fails)
  - `pub async fn SystemdClient::list_units(&self) -> Vec<argus_proto::v1::Unit>`
  - `pub async fn SystemdClient::run_verb(&self, command_id: String, verb: argus_proto::v1::Verb, target: &str) -> argus_proto::v1::CommandResult`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `crates/agent/src/systemd.rs`:

```rust
    /// No system bus in the test environment: the client must degrade to an
    /// empty unit list and a clearly-failed verb rather than panicking. This is
    /// the LXC-without-systemd / container path.
    #[tokio::test]
    async fn client_without_a_bus_reports_empty_and_fails_verbs_cleanly() {
        let client = SystemdClient { inner: None, self_unit: None };
        assert!(client.list_units().await.is_empty());

        let r = client
            .run_verb("c1".into(), argus_proto::v1::Verb::UnitStart, "nginx")
            .await;
        assert!(!r.ok);
        assert!(r.message.contains("systemd"), "message must say why: {}", r.message);
    }

    /// The self-preservation guard must hold before any bus call, so it applies
    /// even on a host with no bus at all.
    #[tokio::test]
    async fn run_verb_refuses_the_agents_own_unit() {
        let client = SystemdClient {
            inner: None,
            self_unit: Some("argus-agent.service".to_string()),
        };
        let r = client
            .run_verb("c1".into(), argus_proto::v1::Verb::UnitStop, "argus-agent")
            .await;
        assert!(!r.ok);
        assert!(
            r.message.contains("refusing"),
            "must explain the refusal, got: {}",
            r.message
        );
    }

    /// Live-bus verb execution. Ignored by default like the repo's other
    /// live-dependency tests; run with `--ignored` on a systemd host.
    #[tokio::test]
    #[ignore = "needs a live D-Bus system bus"]
    async fn live_bus_lists_units() {
        let client = SystemdClient::connect().await;
        let units = client.list_units().await;
        assert!(!units.is_empty(), "a systemd host must report at least one unit");
        assert!(
            units.iter().any(|u| u.name.ends_with(".service")),
            "the *.service query must return something"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-agent systemd`
Expected: FAIL — `cannot find struct 'SystemdClient' in this scope`.

- [ ] **Step 3: Write the implementation**

Replace the import line at the top of `crates/agent/src/systemd.rs` with:

```rust
use argus_proto::v1::{CommandResult, Unit as ProtoUnit, Verb};
use futures_util::StreamExt;
use std::time::Duration;
use zbus::proxy;
use zvariant::OwnedObjectPath;
```

Add the proxy definitions immediately after the `UnitRecord` struct:

```rust
#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Manager {
    fn subscribe(&self) -> zbus::Result<()>;

    fn list_units_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::Result<Vec<UnitRecord>>;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    fn get_unit_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}
```

Add the client below the pure functions (above `#[cfg(test)]`):

```rust
/// Cap any single bus call so a wedged systemd can't stall the Session's
/// heartbeat/metrics sender. Matches `docker.rs`'s OP_TIMEOUT.
const OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Fallback name for the agent's own unit, used only when `GetUnitByPID` fails
/// (i.e. the agent isn't running under systemd, where the guard is moot anyway).
/// Nothing in this repo pins the deployed unit name — the provisioning template
/// owns it (PRD §5.1) — which is exactly why the real value is discovered at
/// runtime rather than hard-coded here.
const FALLBACK_SELF_UNIT: &str = "argus-agent.service";

/// A cheaply-cloneable handle to the local systemd. `inner` is `None` on hosts
/// with no system bus (containers without systemd) — such a client reports an
/// empty unit list and fails verbs with a clear message, never panicking.
#[derive(Clone)]
pub struct SystemdClient {
    inner: Option<zbus::Connection>,
    /// The unit hosting this agent, discovered at connect. Verbs against it are
    /// refused: stopping it severs the session the result would return on.
    self_unit: Option<String>,
}

impl SystemdClient {
    /// Best-effort connect to the system bus. Never fails.
    pub async fn connect() -> SystemdClient {
        let conn = match zbus::Connection::system().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "systemd: no system bus; unit features disabled");
                return SystemdClient { inner: None, self_unit: None };
            }
        };

        // systemd only emits job signals while at least one client is
        // subscribed. Without this, run_verb's JobRemoved wait always times out.
        if let Ok(mgr) = ManagerProxy::new(&conn).await {
            if let Err(e) = mgr.subscribe().await {
                tracing::warn!(error = %e, "systemd: Subscribe failed; verb results may time out");
            }
        }

        let self_unit = discover_self_unit(&conn).await;
        tracing::info!(self_unit = ?self_unit, "systemd: connected to system bus");
        SystemdClient { inner: Some(conn), self_unit }
    }

    /// Loaded `*.service` units plus any failed unit of any type, deduplicated.
    /// Empty on no-bus or on ANY error — a partial list that dropped the failed
    /// query would render as "nothing is wrong on this host", the one wrong
    /// answer this slice must never give.
    pub async fn list_units(&self) -> Vec<ProtoUnit> {
        let Some(conn) = &self.inner else {
            return Vec::new();
        };
        match tokio::time::timeout(OP_TIMEOUT, list_units_inner(conn)).await {
            Ok(Ok(units)) => units,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "systemd: listing units failed");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!("systemd: listing units timed out");
                Vec::new()
            }
        }
    }

    /// Run a unit verb, reporting the real job outcome (not merely that the job
    /// was enqueued).
    pub async fn run_verb(&self, command_id: String, verb: Verb, target: &str) -> CommandResult {
        // Checked before touching the bus so it holds on every host.
        if is_self_unit(target, self.self_unit.as_deref()) {
            return result(
                command_id,
                false,
                "refusing to operate on the unit hosting this agent",
            );
        }
        let Some(conn) = &self.inner else {
            return result(command_id, false, "systemd system bus not available on this host");
        };
        let unit = normalize_target(target);

        match tokio::time::timeout(OP_TIMEOUT, run_verb_inner(conn, verb, &unit)).await {
            Ok(Ok(job_result)) => job_result_to_command_result(command_id, &job_result),
            Ok(Err(e)) => result(command_id, false, &e.to_string()),
            Err(_) => result(
                command_id,
                false,
                "job enqueued; outcome unconfirmed within 5s",
            ),
        }
    }
}

/// Which unit hosts this process, via PID -> unit object path -> its `Id`.
/// Falls back to the compiled-in name when systemd can't tell us.
async fn discover_self_unit(conn: &zbus::Connection) -> Option<String> {
    async fn inner(conn: &zbus::Connection) -> zbus::Result<String> {
        let mgr = ManagerProxy::new(conn).await?;
        let path = mgr.get_unit_by_pid(std::process::id()).await?;
        let unit = UnitProxy::builder(conn).path(path)?.build().await?;
        unit.id().await
    }
    match inner(conn).await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                error = %e,
                fallback = FALLBACK_SELF_UNIT,
                "systemd: could not resolve this agent's own unit; using the fallback name"
            );
            Some(FALLBACK_SELF_UNIT.to_string())
        }
    }
}

/// The two pattern queries, unioned. Any error propagates so the caller can
/// return an empty list rather than a misleading partial one.
async fn list_units_inner(conn: &zbus::Connection) -> zbus::Result<Vec<ProtoUnit>> {
    let mgr = ManagerProxy::new(conn).await?;
    let services = mgr
        .list_units_by_patterns(vec![], vec!["*.service".to_string()])
        .await?;
    let failed = mgr
        .list_units_by_patterns(vec!["failed".to_string()], vec![])
        .await?;
    Ok(merge_units(services, failed))
}

/// Dispatch the verb and wait for ITS job to complete, returning systemd's own
/// result string. The signal stream is opened BEFORE the call — a fast job can
/// otherwise complete before we are listening.
async fn run_verb_inner(
    conn: &zbus::Connection,
    verb: Verb,
    unit: &str,
) -> zbus::Result<String> {
    let mgr = ManagerProxy::new(conn).await?;
    let mut jobs = mgr.receive_job_removed().await?;

    let job_path = match verb {
        Verb::UnitStart => mgr.start_unit(unit, "replace").await?,
        Verb::UnitStop => mgr.stop_unit(unit, "replace").await?,
        Verb::UnitRestart => mgr.restart_unit(unit, "replace").await?,
        other => {
            return Err(zbus::Error::Failure(format!("unsupported verb {other:?}")));
        }
    };

    while let Some(signal) = jobs.next().await {
        let args = signal.args()?;
        if *args.job() == job_path {
            return Ok(args.result().to_string());
        }
    }
    Err(zbus::Error::Failure(
        "job signal stream ended before the job completed".to_string(),
    ))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-agent systemd`
Expected: PASS — 8 tests, 1 ignored.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: clean.

On a systemd host, also run the live test: `cargo test -p argus-agent systemd -- --ignored`
Expected: PASS.

- [ ] **Step 5: Verify the musl build still holds**

Run:
```bash
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `static-pie linked`.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src/systemd.rs
git commit -m "feat(agent): systemd D-Bus client over zbus

Connects to the system bus, Subscribe()s so job signals are emitted, and
discovers the agent's own unit via GetUnitByPID so the self-preservation
guard doesn't depend on a hard-coded name. list_units runs the two pattern
queries and fails closed to an empty list rather than reporting a partial
one. run_verb correlates JobRemoved by job path, so 'ok' means the unit
actually started rather than that systemd accepted the request."
```

---

### Task 4: Agent `session.rs` — emit SystemdState and route unit verbs

**Files:**
- Modify: `crates/agent/src/session.rs:1-20` (imports), `:38-43` (client construction), `:96-99` (clones), `:129-143` (post-Hello snapshot), `:200-213` (tick), `:229-252` (inbound verb routing)

**Interfaces:**
- Consumes from Task 3: `SystemdClient::{connect, list_units, run_verb}`.
- Produces: no new API — behavioral change only.

- [ ] **Step 1: Add the imports and construct the client**

In `crates/agent/src/session.rs`, change the import at line 4-8 from:

```rust
use crate::docker::DockerClient;
use crate::enroll::Identity;
use anyhow::{Context, Result};
use argus_proto::v1::agent_service_client::AgentServiceClient;
use argus_proto::v1::{agent_frame, server_frame, AgentFrame, DockerState, Heartbeat, Hello, Verb};
```

to:

```rust
use crate::docker::DockerClient;
use crate::enroll::Identity;
use crate::systemd::SystemdClient;
use anyhow::{Context, Result};
use argus_proto::v1::agent_service_client::AgentServiceClient;
use argus_proto::v1::{
    agent_frame, server_frame, AgentFrame, DockerState, Heartbeat, Hello, SystemdState, Verb,
};
```

In `run` (line 39), change:

```rust
    let docker = DockerClient::connect();
```

to:

```rust
    let docker = DockerClient::connect();
    let systemd = SystemdClient::connect().await;
```

and change the `connect_and_serve` call on line 43 from:

```rust
        let outcome = connect_and_serve(cfg, &identity, &docker).await;
```

to:

```rust
        let outcome = connect_and_serve(cfg, &identity, &docker, &systemd).await;
```

Change `connect_and_serve`'s signature (line 77) from:

```rust
async fn connect_and_serve(cfg: &Config, identity: &Identity, docker: &DockerClient) -> Result<()> {
```

to:

```rust
async fn connect_and_serve(
    cfg: &Config,
    identity: &Identity,
    docker: &DockerClient,
    systemd: &SystemdClient,
) -> Result<()> {
```

- [ ] **Step 2: Clone the client into both paths**

Change lines 97-99 from:

```rust
    let inbound_tx = tx.clone();
    let inbound_docker = docker.clone();
    let sender_docker = docker.clone();
```

to:

```rust
    let inbound_tx = tx.clone();
    let inbound_docker = docker.clone();
    let sender_docker = docker.clone();
    let inbound_systemd = systemd.clone();
    let sender_systemd = systemd.clone();
```

- [ ] **Step 3: Send the post-Hello snapshot**

Immediately after the initial `DockerState` send block (which ends at line 143 with its closing `}`), and before `let start = tokio::time::Instant::now();`, insert:

```rust
        // Initial systemd snapshot alongside the Docker one, so the Units tab
        // populates promptly rather than a full tick later.
        let units = sender_systemd.list_units().await;
        if tx
            .send(AgentFrame {
                stream_id: argus_common::CONTROL_STREAM_ID,
                payload: Some(agent_frame::Payload::SystemdState(SystemdState { units })),
            })
            .await
            .is_err()
        {
            return;
        }
```

- [ ] **Step 4: Send on every tick**

At the end of the `loop` in the sender task, after the `DockerState` send block (which ends around line 213), insert:

```rust
            let units = sender_systemd.list_units().await;
            if tx
                .send(AgentFrame {
                    stream_id: argus_common::CONTROL_STREAM_ID,
                    payload: Some(agent_frame::Payload::SystemdState(SystemdState { units })),
                })
                .await
                .is_err()
            {
                tracing::debug!(agent_id = %sender_agent_id, "session: systemd sender exiting, channel closed");
                return;
            }
```

- [ ] **Step 5: Route unit verbs on the inbound path**

Replace the whole `tokio::spawn` block inside the `Command` arm (lines 238-249) with:

```rust
                        let docker = inbound_docker.clone();
                        let systemd = inbound_systemd.clone();
                        let out = inbound_tx.clone();
                        tokio::spawn(async move {
                            let verb = Verb::try_from(cmd.verb).unwrap_or(Verb::Unspecified);
                            let result = match verb {
                                Verb::ContainerStart
                                | Verb::ContainerStop
                                | Verb::ContainerRestart => {
                                    docker.run_verb(cmd.command_id.clone(), verb, &cmd.target).await
                                }
                                Verb::UnitStart | Verb::UnitStop | Verb::UnitRestart => {
                                    systemd.run_verb(cmd.command_id.clone(), verb, &cmd.target).await
                                }
                                // Always reply: an unanswered command leaves the
                                // browser's bounded wait to time out into a 202
                                // for a verb that was never going to run.
                                Verb::Unspecified => argus_proto::v1::CommandResult {
                                    command_id: cmd.command_id.clone(),
                                    ok: false,
                                    exit_code: 1,
                                    message: format!("unsupported verb code {}", cmd.verb),
                                },
                            };
                            let _ = out
                                .send(AgentFrame {
                                    stream_id,
                                    payload: Some(agent_frame::Payload::CommandResult(result)),
                                })
                                .await;
                        });
```

Note the `let docker = inbound_docker.clone();` line replaces the existing one — keep `let stream_id = frame.stream_id;` above it unchanged.

- [ ] **Step 6: Verify it builds and the existing tests still pass**

Run: `cargo test -p argus-agent`
Expected: PASS — all existing session backoff tests plus the systemd tests.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/agent/src/session.rs
git commit -m "feat(agent): report SystemdState and execute unit verbs

The systemd snapshot rides the existing 15s sender tick next to DockerState,
and the inbound Command arm now routes by verb family. Unknown verbs get an
explicit failed CommandResult rather than silence, so the control plane's
bounded wait resolves instead of timing out."
```

---

### Task 5: Server `hub.rs` — the systemd snapshot cache

**Files:**
- Modify: `crates/server/src/hub.rs:1-10` (doc + imports), `:33-39` (struct), `:70-83` (add methods after the docker ones), test module

**Interfaces:**
- Consumes: nothing new.
- Produces, for Tasks 6, 7, 8:
  - `pub fn Hub::set_systemd(&self, machine_id: Uuid, units: Vec<argus_proto::v1::Unit>)`
  - `pub fn Hub::get_systemd(&self, machine_id: Uuid) -> Vec<argus_proto::v1::Unit>`
  - `pub fn Hub::failed_unit_count(&self, machine_id: Uuid) -> usize`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/server/src/hub.rs`:

```rust
    fn unit(name: &str, active_state: &str) -> Unit {
        Unit {
            name: name.into(),
            load_state: "loaded".into(),
            active_state: active_state.into(),
            sub_state: "dead".into(),
            description: format!("desc {name}"),
        }
    }

    #[test]
    fn set_then_get_systemd_round_trips_and_defaults_empty() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        assert!(hub.get_systemd(m).is_empty());
        hub.set_systemd(m, vec![unit("nginx.service", "active")]);
        let got = hub.get_systemd(m);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "nginx.service");
    }

    #[test]
    fn failed_unit_count_counts_only_failed_units() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        assert_eq!(hub.failed_unit_count(m), 0, "unknown machine reports zero");

        hub.set_systemd(
            m,
            vec![
                unit("a.service", "active"),
                unit("b.service", "failed"),
                unit("c.mount", "failed"),
                unit("d.service", "inactive"),
            ],
        );
        assert_eq!(hub.failed_unit_count(m), 2);
    }

    #[test]
    fn a_later_snapshot_replaces_the_earlier_one() {
        // The agent re-sends a full snapshot every tick and on reconnect; a
        // resolved failure must not linger in the cache.
        let hub = Hub::new();
        let m = Uuid::new_v4();
        hub.set_systemd(m, vec![unit("b.service", "failed")]);
        assert_eq!(hub.failed_unit_count(m), 1);
        hub.set_systemd(m, vec![unit("b.service", "active")]);
        assert_eq!(hub.failed_unit_count(m), 0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-server hub`
Expected: FAIL — `cannot find type 'Unit' in this scope` / `no method named 'set_systemd'`.

- [ ] **Step 3: Write the implementation**

Change the import on line 8 from:

```rust
use argus_proto::v1::{server_frame, Command, CommandResult, Container, ServerFrame, Verb};
```

to:

```rust
use argus_proto::v1::{server_frame, Command, CommandResult, Container, ServerFrame, Unit, Verb};
```

Add a field to the `Hub` struct (after `docker`, line 36):

```rust
    systemd: Mutex<HashMap<Uuid, Vec<Unit>>>,
```

Add these methods immediately after `get_docker` (line 83):

```rust
    /// Replace the cached unit snapshot for a machine.
    pub fn set_systemd(&self, machine_id: Uuid, units: Vec<Unit>) {
        self.systemd.lock().unwrap().insert(machine_id, units);
    }

    /// The latest cached unit snapshot for a machine (empty if none reported).
    pub fn get_systemd(&self, machine_id: Uuid) -> Vec<Unit> {
        self.systemd
            .lock()
            .unwrap()
            .get(&machine_id)
            .cloned()
            .unwrap_or_default()
    }

    /// How many of a machine's cached units are in the `failed` state. Counted
    /// here rather than in the handler so the fleet query stays one cheap read
    /// under a single lock, and never clones the snapshot.
    pub fn failed_unit_count(&self, machine_id: Uuid) -> usize {
        self.systemd
            .lock()
            .unwrap()
            .get(&machine_id)
            .map(|units| units.iter().filter(|u| u.active_state == "failed").count())
            .unwrap_or(0)
    }
```

Also update the module doc on line 1-2, from `the latest Docker snapshot per machine` to `the latest Docker and systemd snapshots per machine`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-server hub`
Expected: PASS — the 3 new tests plus the 6 existing ones.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/hub.rs
git commit -m "feat(server): cache systemd unit snapshots in the Hub

Mirrors the Docker snapshot map: in-memory, replaced wholesale on every
report, re-derived on reconnect. failed_unit_count lives here so the fleet
query is one cheap read under a single lock."
```

---

### Task 6: Server `grpc.rs` — handle the SystemdState frame

**Files:**
- Modify: `crates/server/src/grpc.rs:270-273` (add the arm after the `DockerState` one), test module

**Interfaces:**
- Consumes from Task 5: `Hub::set_systemd`, `Hub::failed_unit_count`.
- Produces: no new API.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/server/src/grpc.rs`, next to `handle_agent_frame_docker_state_caches_snapshot`:

```rust
    /// A `SystemdState` frame must cache the reported units in the hub, keyed by
    /// the authenticated machine_id, and refresh `last_seen_at`.
    #[sqlx::test]
    async fn handle_agent_frame_systemd_state_caches_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-systemd-1".to_string(),
                hostname: "systemd-host".to_string(),
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
                payload: Some(agent_frame::Payload::SystemdState(
                    argus_proto::v1::SystemdState {
                        units: vec![
                            argus_proto::v1::Unit {
                                name: "nginx.service".into(),
                                load_state: "loaded".into(),
                                active_state: "active".into(),
                                sub_state: "running".into(),
                                description: "A high performance web server".into(),
                            },
                            argus_proto::v1::Unit {
                                name: "backup.service".into(),
                                load_state: "loaded".into(),
                                active_state: "failed".into(),
                                sub_state: "failed".into(),
                                description: "Nightly backup".into(),
                            },
                        ],
                    },
                )),
            },
            &tx,
        )
        .await?;

        let cached = hub.get_systemd(machine_id);
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].name, "nginx.service");
        assert_eq!(hub.failed_unit_count(machine_id), 1);

        let row = sqlx::query!(
            "SELECT last_seen_at FROM machines WHERE id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            row.last_seen_at.is_some(),
            "a SystemdState frame must refresh last_seen_at"
        );

        Ok(())
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p argus-server handle_agent_frame_systemd_state`
Expected: FAIL — the frame falls through to the catch-all arm, so `cached.len()` is 0, not 2.

- [ ] **Step 3: Write the implementation**

In `crates/server/src/grpc.rs`, immediately after the `DockerState` arm (lines 270-273), add:

```rust
        Some(agent_frame::Payload::SystemdState(ss)) => {
            hub.set_systemd(machine_id, ss.units);
            repo::touch_last_seen(pool, machine_id).await?;
        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p argus-server handle_agent_frame_systemd_state`
Expected: PASS.

Run: `cargo test -p argus-server`
Expected: PASS — no regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/grpc.rs
git commit -m "feat(server): cache SystemdState frames from the Session

Symmetric with the DockerState arm: cache under the cert-authenticated
machine_id and refresh last_seen_at."
```

---

### Task 7: Server `http.rs` — generalize the verb handler and add the unit endpoints

The container and unit verb flows are identical apart from their action→`Verb` table and audit prefix. Generalize rather than copy, so they cannot drift.

**Files:**
- Modify: `crates/server/src/http.rs:47-65` (routes), `:287-320` (add the Unit DTO), `:335-438` (the verb handlers), test module

**Interfaces:**
- Consumes from Task 5: `Hub::get_systemd`.
- Produces:
  - `async fn run_verb(state: &AppState, id: Uuid, verb: Verb, target: &str, audit_action: &str, timeout: Duration) -> Response`
  - `async fn machine_systemd(State<AppState>, Path<Uuid>) -> Json<Vec<UnitDto>>`
  - `async fn unit_action(State<AppState>, Path<(Uuid, String, String)>) -> Response`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/server/src/http.rs`:

```rust
    #[sqlx::test]
    async fn get_systemd_returns_cached_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let (state, hub) = app_state_with_hub(pool);
        let id = Uuid::new_v4();

        // empty before any report
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/systemd"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert!(rows.is_empty());

        // populate the cache, then it shows up
        hub.set_systemd(
            id,
            vec![argus_proto::v1::Unit {
                name: "nginx.service".into(),
                load_state: "loaded".into(),
                active_state: "failed".into(),
                sub_state: "failed".into(),
                description: "A high performance web server".into(),
            }],
        );
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/machines/{id}/systemd"))
                    .body(Body::empty())?,
            )
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "nginx.service");
        assert_eq!(rows[0]["active_state"], "failed");

        Ok(())
    }

    #[sqlx::test]
    async fn unit_verb_on_offline_agent_returns_409_and_audits_denied(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('unit-offline', 'h', 'offline') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = run_verb(
            &state,
            machine_id,
            Verb::UnitRestart,
            "nginx.service",
            "unit.restart",
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let row = sqlx::query!(
            "SELECT result, target_ref FROM audit_log WHERE machine_id = $1 AND action = 'unit.restart'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("denied"));
        assert_eq!(row.target_ref.as_deref(), Some("nginx.service"));

        Ok(())
    }

    #[sqlx::test]
    async fn unit_verb_with_connected_agent_completes_ok(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('unit-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: echo a success CommandResult, and record the verb it saw so
        // we can assert a UNIT verb (not a container one) went down the wire.
        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<i32>();
        tokio::spawn(async move {
            let mut seen_tx = Some(seen_tx);
            while let Some(Ok(frame)) = rx.recv().await {
                if let Some(server_frame::Payload::Command(cmd)) = frame.payload {
                    if let Some(s) = seen_tx.take() {
                        let _ = s.send(cmd.verb);
                    }
                    hub2.complete(
                        &cmd.command_id.clone(),
                        machine_id,
                        CommandResult {
                            command_id: cmd.command_id,
                            ok: true,
                            exit_code: 0,
                            message: "done".into(),
                        },
                    );
                }
            }
        });

        let resp = run_verb(
            &state,
            machine_id,
            Verb::UnitStart,
            "nginx.service",
            "unit.start",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "completed");

        let seen = seen_rx.await?;
        assert_eq!(seen, Verb::UnitStart as i32, "a UNIT verb must ride the wire");

        Ok(())
    }

    #[sqlx::test]
    async fn unit_action_rejects_a_malformed_unit_name(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);

        // A unit name is never empty and never contains '/'. `%2F` decodes to a
        // slash inside the path segment, which must not be forwarded to an agent.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/machines/{}/units/{}/start",
                        Uuid::new_v4(),
                        "..%2Fetc%2Fpasswd"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[sqlx::test]
    async fn unit_action_with_unknown_action_returns_400(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/machines/{}/units/nginx.service/obliterate",
                        Uuid::new_v4()
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-server --bin argus http`
Expected: FAIL — `cannot find function 'run_verb' in this scope`, and the routed tests 404 rather than 400.

- [ ] **Step 3: Write the implementation**

**(a) Add the Unit DTO.** After the `ContainerDto` `From` impl (line 310) in `crates/server/src/http.rs`, add:

```rust
/// One unit row for the detail page's units panel, mirroring the proto `Unit`
/// (which isn't `Serialize`).
#[derive(serde::Serialize)]
struct UnitDto {
    name: String,
    load_state: String,
    active_state: String,
    sub_state: String,
    description: String,
}

impl From<argus_proto::v1::Unit> for UnitDto {
    fn from(u: argus_proto::v1::Unit) -> Self {
        UnitDto {
            name: u.name,
            load_state: u.load_state,
            active_state: u.active_state,
            sub_state: u.sub_state,
            description: u.description,
        }
    }
}

/// `GET /api/machines/{id}/systemd` — the machine's latest cached unit list
/// (empty when the agent hasn't reported / has no systemd).
async fn machine_systemd(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Json<Vec<UnitDto>> {
    let units = state.hub.get_systemd(id);
    Json(units.into_iter().map(UnitDto::from).collect())
}
```

**(b) Generalize the verb core.** Replace `container_action` and `run_container_verb` (lines 335-438) with:

```rust
/// `POST /api/machines/{id}/docker/{container}/{action}` — dispatch a container
/// verb and wait up to `VERB_TIMEOUT` for the agent's result.
async fn container_action(
    State(state): State<AppState>,
    Path((id, container, action)): Path<(Uuid, String, String)>,
) -> Response {
    let verb = match action.as_str() {
        "start" => Verb::ContainerStart,
        "stop" => Verb::ContainerStop,
        "restart" => Verb::ContainerRestart,
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    run_verb(
        &state,
        id,
        verb,
        &container,
        &format!("container.{action}"),
        VERB_TIMEOUT,
    )
    .await
}

/// `POST /api/machines/{id}/units/{unit}/{action}` — dispatch a systemd unit
/// verb and wait up to `VERB_TIMEOUT` for the agent's result.
async fn unit_action(
    State(state): State<AppState>,
    Path((id, unit, action)): Path<(Uuid, String, String)>,
) -> Response {
    let verb = match action.as_str() {
        "start" => Verb::UnitStart,
        "stop" => Verb::UnitStop,
        "restart" => Verb::UnitRestart,
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    // A systemd unit name is never empty and never contains '/'. Reject rather
    // than forward something an agent would only fail on anyway.
    if unit.is_empty() || unit.contains('/') {
        return (StatusCode::BAD_REQUEST, "invalid unit name").into_response();
    }
    run_verb(&state, id, verb, &unit, &format!("unit.{action}"), VERB_TIMEOUT).await
}

/// The shared verb pipeline for every verb family: audit-before-dispatch (fail
/// closed), dispatch, then a bounded wait for the agent's result. `timeout` is
/// injected so tests don't wait the full 10s.
async fn run_verb(
    state: &AppState,
    id: Uuid,
    verb: Verb,
    target: &str,
    audit_action: &str,
    timeout: Duration,
) -> Response {
    let actor = "anonymous";
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
        audit_action,
        Some(id),
        target,
        command_id,
        "dispatched",
    )
    .await
    {
        // Fail closed: a verb must never execute unaudited (CLAUDE.md). If the
        // dispatched audit write fails, abandon the waiter and do NOT dispatch.
        state.hub.abandon_pending(&cid);
        tracing::error!(error = %e, action = audit_action, "verb: dispatched audit write failed; not dispatching");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to record audit entry",
        )
            .into_response();
    }

    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_command(id, cid.clone(), verb, target.to_string(), actor.to_string())
        .await
    {
        state.hub.abandon_pending(&cid);
        // The agent is offline: no CommandResult will ever arrive to resolve the
        // row, so flip it to the terminal "denied" state here. This is the one
        // case the grpc CommandResult arm cannot cover (the command was never
        // delivered), so it does not conflict with that arm being the sole
        // writer of a real ok/error result.
        if let Err(e) = repo::update_command_result(&state.pool, command_id, id, "denied").await {
            tracing::error!(error = %e, "verb: denied audit update failed");
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
```

**(c) Register the routes.** In `router` (line 54-58), after the docker routes, add:

```rust
        .route("/api/machines/{id}/systemd", get(machine_systemd))
        .route("/api/machines/{id}/units/{unit}/{action}", post(unit_action))
```

**(d) Fix the pre-existing container tests.** The four tests calling `run_container_verb(&state, id, "web", "restart", timeout)` must move to the new signature. Change each call to `run_verb` with an explicit verb and audit action:

- in `verb_on_offline_agent_returns_409_and_audits_denied`:
  `run_verb(&state, machine_id, Verb::ContainerRestart, "web", "container.restart", Duration::from_millis(200))`
- in `verb_with_connected_agent_completes_ok`:
  `run_verb(&state, machine_id, Verb::ContainerStart, "web", "container.start", Duration::from_secs(5))`
- in `verb_times_out_to_202_when_agent_never_replies`:
  `run_verb(&state, machine_id, Verb::ContainerStop, "web", "container.stop", Duration::from_millis(150))`
- in `verb_fails_closed_when_the_dispatched_audit_write_fails`:
  `run_verb(&state, ghost_id, Verb::ContainerStart, "web", "container.start", Duration::from_millis(200))`

Delete `verb_with_unknown_action_returns_400` entirely — action validation now lives in the *handlers*, not the shared core, so that test's premise is gone. It is replaced by `unit_action_with_unknown_action_returns_400` (added in Step 1) plus this equivalent for containers, which you should add next to it:

```rust
    #[sqlx::test]
    async fn container_action_with_unknown_action_returns_400(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/machines/{}/docker/web/obliterate",
                        Uuid::new_v4()
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-server --bin argus http`
Expected: PASS — all pre-existing http tests plus the 5 new ones.

Run: `cargo clippy -p argus-server --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/http.rs
git commit -m "feat(server): systemd unit endpoints on a shared verb pipeline

GET /api/machines/:id/systemd and POST /api/machines/:id/units/:unit/:action.

run_container_verb becomes run_verb, parameterized by verb and audit action,
so the container and unit paths share one audit-before-dispatch, fail-closed,
bounded-wait implementation instead of a copy that would drift. Each handler
owns only its own action->Verb table; unit names are validated before
dispatch."
```

---

### Task 8: Server `http.rs` — failed-unit count on the fleet row

**Files:**
- Modify: `crates/server/src/http.rs:73-88` (`FleetRow`), `:132-157` (row construction), test module

**Interfaces:**
- Consumes from Task 5: `Hub::failed_unit_count`.
- Produces: `failed_units: usize` on each `/api/fleet` row.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `crates/server/src/http.rs`:

```rust
    #[sqlx::test]
    async fn fleet_reports_failed_unit_counts_from_the_hub(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('fleet-failed', 'fh', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool);

        // No snapshot yet -> 0, never null.
        let app = router(state.clone());
        let resp = app
            .oneshot(Request::builder().uri("/api/fleet").body(Body::empty())?)
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows[0]["failed_units"], 0);

        hub.set_systemd(
            machine_id,
            vec![
                argus_proto::v1::Unit {
                    name: "ok.service".into(),
                    load_state: "loaded".into(),
                    active_state: "active".into(),
                    sub_state: "running".into(),
                    description: String::new(),
                },
                argus_proto::v1::Unit {
                    name: "bad.service".into(),
                    load_state: "loaded".into(),
                    active_state: "failed".into(),
                    sub_state: "failed".into(),
                    description: String::new(),
                },
            ],
        );

        let app = router(state);
        let resp = app
            .oneshot(Request::builder().uri("/api/fleet").body(Body::empty())?)
            .await?;
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
        assert_eq!(rows[0]["failed_units"], 1);

        Ok(())
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p argus-server --bin argus fleet_reports_failed_unit_counts`
Expected: FAIL — `failed_units` is `Null`, not `0`.

- [ ] **Step 3: Write the implementation**

Add the field to `FleetRow` (after `mem_pct`, line 85):

```rust
    /// Count of units in the `failed` state from the live Hub snapshot. `0` for a
    /// machine that is offline or has never reported — the UI only renders the
    /// chip when this is `> 0`, and an offline machine already carries its own
    /// status badge, so `0` is never read as "healthy" on a machine we can't see.
    failed_units: usize,
```

In the row-construction closure, add before `FleetRow {`:

```rust
            let failed_units = state.hub.failed_unit_count(r.id);
```

and add `failed_units,` to the struct literal.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/http.rs
git commit -m "feat(server): failed-unit count on the fleet row

Reads the live Hub snapshot alongside the existing fleet query so 'which
machine has a problem' is answerable without opening every machine."
```

---

### Task 9: Frontend data layer — types, fetchers, hooks, and the sort/filter helper

**Files:**
- Modify: `frontend/src/api.ts` (add `Unit`, `getSystemd`, `UnitAction`, `unitAction`, `failed_units` on `FleetRow`)
- Modify: `frontend/src/lib/queries.ts` (add `qk.systemd`, `useSystemd`, `useUnitAction`)
- Modify: `frontend/src/lib/status.ts` (add `unitTone`)
- Create: `frontend/src/lib/units.ts` (the pure sort/filter helper)

**Interfaces:**
- Consumes from Tasks 7 and 8: `GET /api/machines/:id/systemd`, `POST /api/machines/:id/units/:unit/:action`, `failed_units` on `/api/fleet`.
- Produces, for Tasks 10 and 11:
  - `type Unit = { name, load_state, active_state, sub_state, description }`
  - `type UnitAction = "start" | "stop" | "restart"`
  - `getSystemd(id: string): Promise<Unit[]>`
  - `unitAction(id: string, unit: string, action: UnitAction): Promise<VerbResult>`
  - `useSystemd(id: string)`, `useUnitAction(id: string)`
  - `unitTone(activeState: string): Tone`
  - `visibleUnits(units: Unit[], filter: string, failedOnly: boolean): Unit[]`
  - `countFailed(units: Unit[]): number`
  - `failed_units: number` on `FleetRow`

- [ ] **Step 1: Add the API types and fetchers**

Append to `frontend/src/api.ts`:

```ts
export type Unit = {
  name: string;
  load_state: string;
  active_state: string;
  sub_state: string;
  description: string;
};

export async function getSystemd(id: string): Promise<Unit[]> {
  const r = await fetch(`/api/machines/${id}/systemd`);
  if (!r.ok) throw new Error(`systemd ${r.status}`);
  return r.json();
}

export type UnitAction = "start" | "stop" | "restart";

export async function unitAction(
  id: string,
  unit: string,
  action: UnitAction,
): Promise<VerbResult> {
  const r = await fetch(
    `/api/machines/${id}/units/${encodeURIComponent(unit)}/${action}`,
    { method: "POST" },
  );
  // 200 (completed) and 202 (pending) both carry a VerbResult body; 4xx/5xx
  // (e.g. 409 agent offline) are surfaced as errors.
  if (!r.ok) throw new Error(`action failed: ${r.status}`);
  return r.json();
}
```

And add to the `FleetRow` type (after `mem_pct`):

```ts
  failed_units: number;
```

- [ ] **Step 2: Add the query hooks**

In `frontend/src/lib/queries.ts`, extend the imports:

```ts
import {
  containerAction,
  getDocker,
  getFleet,
  getMachine,
  getMetrics,
  getSystemd,
  unitAction,
} from "../api";
import type { ContainerAction, UnitAction } from "../api";
```

Add to `qk`:

```ts
  systemd: (id: string) => ["systemd", id] as const,
```

Append the hooks:

```ts
export function useSystemd(id: string) {
  return useQuery({
    queryKey: qk.systemd(id),
    queryFn: () => getSystemd(id),
    refetchInterval: MACHINE_INTERVAL,
  });
}

/**
 * Unit verbs. Mirrors useContainerAction: on success the systemd snapshot is
 * invalidated so the table reflects the new state without waiting for the next
 * poll, and per-row in-flight state comes from `variables`.
 */
export function useUnitAction(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { unit: string; action: UnitAction }) =>
      unitAction(id, vars.unit, vars.action),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: qk.systemd(id) });
    },
  });
}
```

- [ ] **Step 3: Add the tone mapping**

In `frontend/src/lib/status.ts`, after `containerTone`:

```ts
/** systemd ActiveState (active|failed|inactive|activating|deactivating|reloading). */
export function unitTone(activeState: string): Tone {
  switch (activeState) {
    case "active":
      return "ok";
    case "activating":
    case "deactivating":
    case "reloading":
      return "warn";
    case "failed":
      return "fail";
    default:
      return "idle";
  }
}
```

- [ ] **Step 4: Add the pure sort/filter helper**

Create `frontend/src/lib/units.ts`:

```ts
// Pure presentation logic for the units table, kept out of the component so it
// can be reasoned about (and tested, once a runner exists) on its own.
import type { Unit } from "../api";

/** Sort rank: failures first, then active, then everything else. */
function rank(u: Unit): number {
  if (u.active_state === "failed") return 0;
  if (u.active_state === "active") return 1;
  return 2;
}

/** How many units are in the failed state. */
export function countFailed(units: Unit[]): number {
  return units.filter((u) => u.active_state === "failed").length;
}

/**
 * The rows to render: optionally narrowed to failures, optionally filtered by a
 * case-insensitive substring of the name or description, then sorted
 * failed → active → other and alphabetically within each group.
 */
export function visibleUnits(
  units: Unit[],
  filter: string,
  failedOnly: boolean,
): Unit[] {
  const needle = filter.trim().toLowerCase();
  return units
    .filter((u) => !failedOnly || u.active_state === "failed")
    .filter(
      (u) =>
        needle === "" ||
        u.name.toLowerCase().includes(needle) ||
        u.description.toLowerCase().includes(needle),
    )
    .slice()
    .sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}
```

- [ ] **Step 5: Verify it typechecks**

Run: `npm --prefix frontend run typecheck`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add frontend/src/api.ts frontend/src/lib/queries.ts frontend/src/lib/status.ts frontend/src/lib/units.ts
git commit -m "feat(frontend): systemd data layer

Unit types and fetchers, TanStack Query hooks mirroring the container ones,
a unitTone mapping, and the units table's sort/filter as a pure module."
```

---

### Task 10: Frontend — the Units tab

**Files:**
- Create: `frontend/src/components/UnitsCard.tsx`
- Modify: `frontend/src/pages/MachineDetailPage.tsx` (import, query, tab entry, panel)

**Interfaces:**
- Consumes from Task 9: `Unit`, `useSystemd`, `useUnitAction`, `unitTone`, `visibleUnits`, `countFailed`.
- Produces: `<UnitsCard machineId={string} units={Unit[]} />`

- [ ] **Step 1: Create the component**

Create `frontend/src/components/UnitsCard.tsx`:

```tsx
// systemd unit list + start/stop/restart verbs for a single machine. A host
// reports far more units than containers, so this table leads with failures and
// carries its own filter — see lib/units.ts for the (pure) ordering rules.
import { useState } from "react";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  ButtonGroup,
  Checkbox,
  EmptyState,
  Input,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { Unit } from "../api";
import { useUnitAction } from "../lib/queries";
import { unitTone } from "../lib/status";
import { countFailed, visibleUnits } from "../lib/units";
import AssetTag from "./AssetTag";
import StatusBadge from "./StatusBadge";

export default function UnitsCard({
  machineId,
  units,
}: {
  machineId: string;
  units: Unit[];
}) {
  const action = useUnitAction(machineId);
  const actionError = action.error;
  const [filter, setFilter] = useState("");
  const [failedOnly, setFailedOnly] = useState(false);

  const failed = countFailed(units);
  const rows = visibleUnits(units, filter, failedOnly);

  return (
    <>
      <div className="flex flex-wrap items-baseline gap-2 pb-2">
        <h2 className="font-display text-sm uppercase tracking-widest">Units</h2>
        <span className="font-mono text-[11px] text-muted-foreground normal-case tracking-normal">
          {units.length} unit{units.length === 1 ? "" : "s"}
          {failed > 0 && ` · ${failed} failed`}
        </span>
        <span className="font-mono text-[11px] text-muted-foreground">
          systemd services on this host, plus any failed unit
        </span>
      </div>

      {actionError != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Action failed</AlertTitle>
          <AlertDescription>{actionError.message}</AlertDescription>
        </Alert>
      )}

      <div className="flex flex-wrap items-center gap-3 pb-2">
        <Input
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="filter units…"
          aria-label="Filter units by name or description"
          className="max-w-xs font-mono text-xs"
        />
        <div className="flex items-center gap-2">
          <Checkbox
            id="units-failed-only"
            checked={failedOnly}
            onCheckedChange={(checked) => setFailedOnly(checked)}
          />
          <label
            htmlFor="units-failed-only"
            className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
          >
            Failed only
          </label>
        </div>
      </div>

      <div className="border-2 border-border">
        {units.length === 0 ? (
          <EmptyState
            title="No units"
            description="This host reported no systemd units (or has no systemd)."
          />
        ) : rows.length === 0 ? (
          <EmptyState
            title="No matching units"
            description="No unit matches the current filter."
          />
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Active</TableHead>
                <TableHead>Sub</TableHead>
                <TableHead>Description</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((u) => {
                const active = u.active_state === "active";
                const rowBusy =
                  action.isPending && action.variables?.unit === u.name;
                return (
                  <TableRow key={u.name}>
                    <TableCell className="font-medium">
                      <AssetTag tone={unitTone(u.active_state)}>{u.name}</AssetTag>
                    </TableCell>
                    <TableCell>
                      <StatusBadge
                        tone={unitTone(u.active_state)}
                        label={u.active_state}
                      />
                    </TableCell>
                    <TableCell className="font-mono text-muted-foreground">
                      {u.sub_state}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {u.description}
                    </TableCell>
                    <TableCell className="text-right">
                      <ButtonGroup>
                        {active ? (
                          <>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() =>
                                action.mutate({ unit: u.name, action: "restart" })
                              }
                            >
                              {rowBusy ? "…" : "Restart"}
                            </Button>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() =>
                                action.mutate({ unit: u.name, action: "stop" })
                              }
                            >
                              {rowBusy ? "…" : "Stop"}
                            </Button>
                          </>
                        ) : (
                          <>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() =>
                                action.mutate({ unit: u.name, action: "start" })
                              }
                            >
                              {rowBusy ? "…" : "Start"}
                            </Button>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() =>
                                action.mutate({ unit: u.name, action: "restart" })
                              }
                            >
                              {rowBusy ? "…" : "Restart"}
                            </Button>
                          </>
                        )}
                      </ButtonGroup>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </div>
    </>
  );
}
```

**Both `Input` and `Checkbox` were confirmed exported during planning**, and their prop shapes matter:
- `Input` is `React.ComponentProps<'input'>` — a normal `onChange` with `e.target.value`.
- `Checkbox` is a **base-ui `Checkbox.Root`**, so it takes `checked` + `onCheckedChange(checked: boolean)` — **not** `onChange`/`e.target.checked`. It renders a button rather than an `<input>`, which is why the label is associated via `id`/`htmlFor` instead of wrapping. Getting this wrong typechecks in some shapes but silently never toggles.

Project memory records that hand-rolling chrome the library already provides has been a repeated mistake here — check `node_modules/@e412/rnui-react/dist/index.d.ts` before substituting a raw element.

- [ ] **Step 2: Wire it into the machine page**

In `frontend/src/pages/MachineDetailPage.tsx`:

Add the import next to `ContainersCard` (line 27):

```tsx
import UnitsCard from "../components/UnitsCard";
```

Extend the queries import to include `useSystemd`, then after line 98 add:

```tsx
  const systemdQuery = useSystemd(id as string);
```

After line 102 add:

```tsx
  const units = systemdQuery.data ?? [];
```

Change line 105 to include the new query's error:

```tsx
  const error =
    machineQuery.error ?? metricsQuery.error ?? dockerQuery.error ?? systemdQuery.error;
```

Add the tab entry (line 216):

```tsx
          { key: "units", label: "Units" },
```

And add the panel after the containers panel (line 322):

```tsx
      {tab === "units" && (
        <div
          role="tabpanel"
          id="panel-units"
          aria-labelledby="tab-units"
          tabIndex={0}
          className="mt-4"
        >
          <UnitsCard machineId={id} units={units} />
        </div>
      )}
```

- [ ] **Step 3: Verify it typechecks and builds**

Run: `npm --prefix frontend run typecheck && npm --prefix frontend run build`
Expected: no type errors; the build emits `frontend/dist`.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/components/UnitsCard.tsx frontend/src/pages/MachineDetailPage.tsx
git commit -m "feat(frontend): units tab with failed-first table and filter

Failures sort to the top, a filter box and failed-only toggle narrow the
list, and per-row verbs mirror the containers tab. All client-side over the
cached snapshot."
```

---

### Task 11: Frontend — failed-unit chip on the fleet page

**Files:**
- Modify: `frontend/src/pages/FleetPage.tsx:35-42` (`StatusCell`)

**Interfaces:**
- Consumes from Tasks 8 and 9: `failed_units` on `FleetRow`.
- Produces: nothing downstream.

- [ ] **Step 1: Add the chip**

In `frontend/src/pages/FleetPage.tsx`, replace `StatusCell` (lines 35-42) with:

```tsx
function StatusCell({ row }: { row: FleetRow }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <StatusBadge tone={machineTone(row.status)} label={row.status} />
      {isReconnecting(row) && <StatusBadge tone="warn" label="reconnecting…" />}
      {row.failed_units > 0 && (
        <StatusBadge
          tone="fail"
          label={`${row.failed_units} failed unit${row.failed_units === 1 ? "" : "s"}`}
        />
      )}
    </div>
  );
}
```

- [ ] **Step 2: Verify it typechecks and builds**

Run: `npm --prefix frontend run typecheck && npm --prefix frontend run build`
Expected: no type errors; build succeeds.

- [ ] **Step 3: Commit**

```bash
git add frontend/src/pages/FleetPage.tsx
git commit -m "feat(frontend): failed-unit chip on the fleet page

Makes 'which machine has a problem' visible from the scan view instead of
requiring a click into each machine."
```

---

### Task 12: Full verification and manual E2E

No new code. This is the gate before the PR.

**Files:**
- Modify: `docs/DEV.md` (append a systemd-slice verification section)

- [ ] **Step 1: Run the whole workspace**

```bash
npm --prefix frontend run build      # rust-embed embeds frontend/dist — build FIRST
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```
Expected: all pass. (Postgres must be up: the `argus-pg` container.)

- [ ] **Step 2: Confirm the musl gate one final time**

```bash
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `static-pie linked`.

- [ ] **Step 3: Manual E2E against a real agent**

Follow `docs/DEV.md`'s run recipe (control plane on `ARGUS_HTTP_ADDR=0.0.0.0:8090`, then the agent). Verify, and record the actual output:

1. `curl -s localhost:8090/api/machines/<id>/systemd | jq 'length'` → a plausible unit count (tens, not hundreds).
2. `curl -s localhost:8090/api/machines/<id>/systemd | jq -r '.[].name' | grep -vc '\.service$'` → 0 unless a non-service unit is genuinely failed.
3. Pick a **safe, disposable** unit to operate on. Do **not** use `sshd`, `dbus`, `systemd-networkd`, or anything the host depends on. If no safe unit exists, create one:
   ```bash
   sudo tee /etc/systemd/system/argus-verify-test.service >/dev/null <<'UNIT'
   [Unit]
   Description=Argus systemd slice verification target
   [Service]
   Type=simple
   ExecStart=/bin/sleep infinity
   UNIT
   sudo systemctl daemon-reload
   ```
4. `curl -X POST localhost:8090/api/machines/<id>/units/argus-verify-test.service/start` → `{"ok":true,...,"status":"completed"}`, and `systemctl is-active argus-verify-test` → `active`.
5. Repeat for `stop` and `restart`, confirming real state changes each time.
6. **Verify the job-correlation actually works** — point a verb at a unit that fails to start and confirm `ok:false` rather than a false success:
   ```bash
   sudo tee /etc/systemd/system/argus-verify-fail.service >/dev/null <<'UNIT'
   [Unit]
   Description=Argus systemd slice failure-path target
   [Service]
   Type=oneshot
   ExecStart=/bin/false
   UNIT
   sudo systemctl daemon-reload
   ```
   `curl -X POST .../units/argus-verify-fail.service/start` → `{"ok":false,...}` with a message carrying systemd's result string. **This is the assertion that distinguishes this slice from a naive enqueue-and-return implementation** — if it returns `ok:true`, the `JobRemoved` correlation is broken.
7. **Verify the self-preservation guard:** `curl -X POST .../units/argus-agent.service/stop` → `{"ok":false, "message":"refusing to operate on the unit hosting this agent"}` and the agent is **still connected** afterwards.
8. `psql` the audit log: `SELECT action, target_ref, result FROM audit_log WHERE action LIKE 'unit.%' ORDER BY at;` → a row per verb with the right terminal result.
9. Browser at `:8090`: the Units tab lists units failures-first, the filter narrows, "failed only" works, buttons act, and the fleet page shows the failed-unit chip.
10. Clean up the test units:
    ```bash
    sudo rm /etc/systemd/system/argus-verify-test.service /etc/systemd/system/argus-verify-fail.service
    sudo systemctl daemon-reload
    ```

- [ ] **Step 4: Record the verification in DEV.md**

Append a "Systemd slice — manual verification" section to `docs/DEV.md` in the same style as the existing Docker slice section: what was run, against which units, and the actual observed results (including the failure-path and self-guard checks).

- [ ] **Step 5: Commit and open the PR**

```bash
git add docs/DEV.md
git commit -m "docs: record systemd slice manual verification"
git push -u origin systemd-slice
fj pr create "feat(server): systemd unit state + start/stop/restart verbs" \
  --base main --head systemd-slice --body-file <(cat <<'BODY'
Slice 4 of 6 (PRD §8). Design: `docs/superpowers/specs/2026-07-23-systemd-slice-design.md`.

Agents report loaded `*.service` units plus any failed unit of any type over the
existing Session stream; the machine page lists them failures-first with a filter
and per-unit start/stop/restart; the fleet page shows a failed-unit count.

Notable decisions:
- verb success means the systemd **job finished** (`JobRemoved` correlation), not
  that it was enqueued — a unit whose ExecStart fails reports `ok:false`
- the agent refuses verbs against its own unit, discovered at runtime via
  `GetUnitByPID` rather than a hard-coded name
- `run_container_verb` generalized to `run_verb` so container and unit verbs
  share one audit-before-dispatch, fail-closed pipeline
- no migration: unit state is an in-memory Hub cache, like Docker's

Agent still builds `static-pie` for `x86_64-unknown-linux-musl`; zbus is pinned
tokio-only so no second async runtime enters the tree.
BODY
)
```

Note: PR titles use conventional-commits format on this repo.

---

## Plan self-review

**Spec coverage** — every section of the design maps to a task:

| Spec section | Task |
|---|---|
| Build gate (zbus/musl) | 1 (done, `e5cd283`) |
| Agent `systemd.rs` pure mapping, normalization, self-unit check, job-result mapping | 2 |
| Agent `systemd.rs` connect / Subscribe / `GetUnitByPID` / `list_units` / `run_verb` | 3 |
| Agent `session.rs` SystemdState frames + verb routing | 4 |
| Server `hub.rs` systemd cache + `failed_unit_count` | 5 |
| Server `grpc.rs` SystemdState arm | 6 |
| Server `http.rs` GET systemd, POST unit verb, `run_verb` generalization | 7 |
| Server fleet `failed_units` | 8 |
| Frontend api/queries/status/units helper | 9 |
| Frontend UnitsCard + tab | 10 |
| Frontend fleet chip | 11 |
| Testing + manual E2E | throughout, gated in 12 |

**Deliberate deviation from the spec, called out rather than silently absorbed:** the spec's testing section says "the sort/filter predicate is a pure exported function so it is testable independently of the component." There is no frontend test runner in this repo (no vitest, no `test` script), and adding one is a tooling decision outside this slice. Task 9 therefore makes the predicate pure and exported — satisfying "testable" — but ships no frontend unit test. Frontend verification is typecheck + build + manual E2E, consistent with all four merged PRs.

**Interface consistency check:** `set_systemd`/`get_systemd`/`failed_unit_count` (Task 5) are used with those exact names in Tasks 6, 7, 8. `SystemdClient::{connect, list_units, run_verb}` (Task 3) match Task 4's call sites, and `connect()` is `async` at both. `run_verb`'s server-side signature (Task 7) matches every call in its own tests and the four rewritten container tests. `Unit`/`UnitAction`/`getSystemd`/`unitAction`/`useSystemd`/`useUnitAction`/`unitTone`/`visibleUnits`/`countFailed` (Task 9) match Task 10's imports exactly. `failed_units` is `usize` server-side (Task 8) and `number` in TS (Task 9), rendered in Task 11.

**Known ripple, handled explicitly:** Task 7 changes `run_container_verb`'s signature, which breaks four existing tests and obsoletes a fifth. Step 3(d) rewrites all five rather than leaving the implementer to discover the breakage.
