# Capability Reporting + Log-Window Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the log time-window a single cutoff shared by a tail and its pages, then have the agent report which subsystems it actually has so the UI can disable surfaces a host cannot support.

**Architecture:** Part 1 — the server resolves the window once at stream open and announces it in a leading SSE `meta` frame; the client echoes it as an absolute `since_ms` on page reads. Part 2 — the agent probes systemd/docker/journal once per session and reports a capability set on `AgentInfo` (which already rides `Enroll` and `Hello`); the control plane stores it in a **nullable** `text[]` and the UI disables gated tabs.

**Tech Stack:** Rust (tonic/prost, tokio, axum, sqlx), React + TS, `@e412/rnui-react` on Tailwind v4.

## Global Constraints

- **Design of record:** `docs/superpowers/specs/2026-07-24-capability-reporting-design.md`. Read it before starting.
- **Proto is additive only.** `capabilities = 8` and `capabilities_reported = 9` on `AgentInfo`. Never renumber or reuse a field number.
- **The capability tri-state is the whole point and must survive end to end:** `capabilities_reported=false` → store **`NULL`** → **gate nothing**; `reported=true` with an empty set → store `{}` → **gate everything**; `reported=true` with entries → gate the rest. Treating `NULL` as "supports nothing" would blank every tab on a working machine the moment an older agent connects — that is the single worst failure this feature can produce.
- **Capability names come from `argus-common` constants** (`CAP_SYSTEMD`, `CAP_DOCKER`, `CAP_JOURNAL`). Neither the agent nor the server may spell a capability as a string literal.
- **Docker's capability requires a real `ping()`.** `Docker::connect_with_socket_defaults()` never contacts the daemon, so `inner.is_some()` would claim `docker` on a host where dockerd is installed but **stopped**.
- **Every probe is timeout-bounded**, and a timeout reports the capability **absent**. A capability probe must never stall session open — the agent's self-healing depends on reconnect being reliable.
- **`since_ms` takes precedence over `window`** on `logs/page`. `logs/stream` accepts `window` only.
- **`since_ms` must be a non-negative integer**, else `400`, consistent with the existing `priority`/`window` validation.
- **Migrations are embedded and run on startup.** New file `0003_capabilities.sql`; never edit `0001`/`0002`.
- **Agent stays lean and musl-static.** No new dependencies; `journalctl` spawned argv-only, never through a shell.
- **Host-dependent tests MUST be named `live_*`** — CI runs `cargo test --workspace -- --ignored --skip live_`.
- **Frontend must build before the server** (`rust-embed` embeds `frontend/dist`).

## File Structure

| File | Responsibility |
|---|---|
| `crates/server/src/http.rs` | `since_ms` query param, `meta` SSE frame, `capabilities` on the detail DTO |
| `frontend/src/api.ts` | `since_ms` on `fetchLogPage`; `capabilities` on the machine type |
| `frontend/src/components/LogViewer.tsx` | listen for `meta`, hold the resolved cutoff, echo it |
| `crates/proto/proto/argus.proto` | +2 additive fields on `AgentInfo` |
| `crates/common/src/lib.rs` | `CAP_*` name constants |
| `crates/agent/src/capabilities.rs` | **new** — the three probes and the set builder |
| `crates/agent/src/session.rs` | probe once per session, attach to `Hello` |
| `crates/server/migrations/0003_capabilities.sql` | **new** — nullable `capabilities text[]` |
| `crates/server/src/repo.rs` | persist capabilities on both write paths |
| `crates/server/src/grpc.rs` | carry capabilities through `agent_info_row` |
| `frontend/src/components/Tabs.tsx` | `disabled` + `reason`, skip disabled in arrow-nav |
| `frontend/src/pages/MachineDetailPage.tsx` | map capabilities → gated tabs, extend the `?tab=` guard |

---

### Task 1: Server — resolved cutoff on the wire

**Files:**
- Modify: `crates/server/src/http.rs` (`LogPageQuery`, `logs_page`, `log_stream`)

**Interfaces:**
- Produces: `logs/page` accepts `since_ms` (wins over `window`); `logs/stream` emits a leading SSE event named `meta` with body `{"since_ms":<n>}`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/server/src/http.rs`:

```rust
    #[test]
    fn explicit_since_ms_overrides_the_window() {
        // A page must be able to say "use the cutoff my stream was given".
        let f = resolve_log_filters_with_since(None, Some("1h"), Some(1_600_000_000_000))
            .expect("an explicit since_ms is valid");
        assert_eq!(f.since_ms, 1_600_000_000_000);
        assert!(!f.current_boot, "an explicit cutoff is not a boot window");
    }

    #[test]
    fn without_since_ms_the_window_still_resolves() {
        let f = resolve_log_filters_with_since(None, Some("boot"), None)
            .expect("boot is a valid window");
        assert!(f.current_boot);
        assert_eq!(f.since_ms, 0);
    }

    #[test]
    fn an_invalid_window_is_still_rejected_even_with_since_ms() {
        assert!(resolve_log_filters_with_since(None, Some("bogus"), Some(1)).is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p argus-server explicit_since_ms_overrides`
Expected: FAIL — `cannot find function resolve_log_filters_with_since`.

- [ ] **Step 3: Add the since-aware resolver**

In `crates/server/src/http.rs`, add next to `resolve_log_filters`:

```rust
/// `resolve_log_filters` plus an explicit, already-resolved cutoff.
///
/// A page read must use the SAME cutoff its stream was given, otherwise every
/// request re-resolves `now` and a view open longer than its own window finds
/// each page fully truncated — reporting "beginning of window" while still
/// displaying a longer span. The stream announces its resolved cutoff in a
/// `meta` frame and the client echoes it here, so `since_ms` wins over `window`.
/// `window` is still validated even when overridden, so a malformed request is
/// rejected rather than silently accepted.
fn resolve_log_filters_with_since(
    priority: Option<u32>,
    window: Option<&str>,
    since_ms: Option<u64>,
) -> Option<LogFilters> {
    let mut f = resolve_log_filters(priority, window)?;
    if let Some(explicit) = since_ms {
        f.since_ms = explicit;
        f.current_boot = false;
    }
    Some(f)
}
```

- [ ] **Step 4: Accept the parameter on the page endpoint**

Add the field to `LogPageQuery`:

```rust
struct LogPageQuery {
    source: String,
    before: Option<String>,
    limit: Option<u32>,
    priority: Option<u32>,
    window: Option<String>,
    since_ms: Option<u64>,
}
```

In `logs_page`, replace the `resolve_log_filters(...)` guard with:

```rust
    let Some(filters) = resolve_log_filters_with_since(q.priority, q.window.as_deref(), q.since_ms)
    else {
        return (StatusCode::BAD_REQUEST, "invalid priority, window or since_ms").into_response();
    };
```

`since_ms` is `Option<u64>`, so axum's `Query` rejects a negative or non-numeric value with `400` before the handler runs — that satisfies the "non-negative integer, else 400" constraint without a manual check.

- [ ] **Step 5: Emit the meta frame**

In `log_stream`, replace the `Sse::new(stream)` construction with:

```rust
    // Announce the resolved cutoff FIRST, so every page read for this tail can
    // echo it back instead of re-resolving `now` and drifting. A *named* event
    // deliberately: the browser's EventSource routes named events to
    // addEventListener and NOT to onmessage, so this frame cannot be mistaken
    // for a log line by the existing NDJSON parsing.
    let meta = Event::default()
        .event("meta")
        .data(format!(r#"{{"since_ms":{}}}"#, filters.since_ms));
    let head = tokio_stream::once(Ok::<Event, Infallible>(meta));

    let stream = ReceiverStream::new(rx).map(move |chunk| {
        // The guard is owned by the closure, so it drops with the stream.
        let _ = &guard;
        Ok::<Event, Infallible>(Event::default().data(String::from_utf8_lossy(&chunk.data)))
    });

    Sse::new(head.chain(stream))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
```

`tokio_stream::StreamExt` is already imported in this file, which is what provides `.chain`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p argus-server` then `cargo clippy -p argus-server --all-targets -- -D warnings`
Expected: all pass, no warnings. If `.sqlx` complains, run `cargo sqlx prepare --workspace -- --all-targets`.

- [ ] **Step 7: Commit**

```bash
git add crates/server/src/http.rs
git commit -m "fix(server): announce the resolved log cutoff and accept it back"
```

---

### Task 2: Frontend — echo the resolved cutoff

**Files:**
- Modify: `frontend/src/api.ts` (`fetchLogPage`)
- Modify: `frontend/src/components/LogViewer.tsx`

**Interfaces:**
- Consumes: the `meta` SSE event and the `since_ms` page param (Task 1).
- Produces: `fetchLogPage(id, source, before, filters, sinceMs?, limit?)`.

- [ ] **Step 1: Accept the cutoff in the API helper**

In `frontend/src/api.ts`, change `fetchLogPage` to:

```ts
export async function fetchLogPage(
  id: string,
  source: string,
  before: string,
  filters: LogFilters = ALL_LOGS,
  sinceMs?: number,
  limit = 500,
): Promise<LogPage> {
  const params = new URLSearchParams({
    source,
    before,
    limit: String(limit),
    ...filterParams(filters),
  });
  // The cutoff the stream resolved for this tail. Sending it keeps every page
  // on the SAME window as the tail; without it the server re-resolves `now` per
  // request and a view open longer than its own window pages into nothing.
  if (sinceMs !== undefined) params.set("since_ms", String(sinceMs));
  const r = await fetch(`/api/machines/${id}/logs/page?${params.toString()}`);
  if (!r.ok) throw new Error(`log page ${r.status}`);
  return (await r.json()) as LogPage;
}
```

- [ ] **Step 2: Capture the meta frame in the viewer**

In `frontend/src/components/LogViewer.tsx`, add a ref beside the existing ones:

```tsx
  // The cutoff this tail resolved, learned from the stream's `meta` frame.
  // A ref, not state: it must be readable by `loadOlder` without making the
  // stream effect depend on it and tear the EventSource down.
  const sinceMsRef = useRef<number | undefined>(undefined);
```

Inside the `EventSource` effect, reset it and subscribe to the named event. Add this immediately after `const es = new EventSource(...)`:

```tsx
    sinceMsRef.current = undefined;
    // Named events do NOT reach `onmessage`, so this cannot collide with the
    // NDJSON log frames. A reconnect re-announces a freshly resolved cutoff and
    // we deliberately adopt it: a reconnected tail is a new read.
    es.addEventListener("meta", (e) => {
      try {
        const m = JSON.parse((e as MessageEvent).data) as { since_ms?: number };
        sinceMsRef.current = typeof m.since_ms === "number" ? m.since_ms : undefined;
      } catch {
        // A malformed meta frame just means we page without an explicit cutoff.
      }
    });
```

- [ ] **Step 3: Echo it on page reads**

In `loadOlder`, change the fetch call to:

```tsx
      const page = await fetchLogPage(machineId, source, oldest, filters, sinceMsRef.current);
```

- [ ] **Step 4: Typecheck and build**

Run: `cd frontend && node_modules/.bin/tsc --noEmit && npm run build`
Expected: exit 0, build succeeds.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/api.ts frontend/src/components/LogViewer.tsx
git commit -m "fix(frontend): page against the cutoff the tail resolved"
```

---

### Task 3: Proto fields, capability names, and the agent probe

**Files:**
- Modify: `crates/proto/proto/argus.proto` (`AgentInfo`)
- Modify: `crates/common/src/lib.rs`
- Create: `crates/agent/src/capabilities.rs`
- Modify: `crates/agent/src/main.rs` (register the module)

**Interfaces:**
- Produces:
  - proto `repeated string capabilities = 8;` and `bool capabilities_reported = 9;`
  - `argus_common::{CAP_SYSTEMD, CAP_DOCKER, CAP_JOURNAL}`
  - `pub async fn probe(systemd: &SystemdClient, docker: &DockerClient) -> Vec<String>`
  - `pub fn build_set(has_systemd: bool, has_docker: bool, has_journal: bool) -> Vec<String>`

- [ ] **Step 1: Write the failing tests**

Create `crates/agent/src/capabilities.rs` containing only its test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_set_lists_only_what_is_present() {
        assert_eq!(
            build_set(true, false, true),
            vec![
                argus_common::CAP_SYSTEMD.to_string(),
                argus_common::CAP_JOURNAL.to_string()
            ]
        );
    }

    #[test]
    fn build_set_is_empty_when_nothing_is_available() {
        // A bare host (e.g. an Alpine LXC) reports an EMPTY set, which is
        // meaningfully different from an old agent reporting nothing at all --
        // the server distinguishes them via `capabilities_reported`.
        assert!(build_set(false, false, false).is_empty());
    }

    #[test]
    fn build_set_uses_the_shared_constants_not_literals() {
        let all = build_set(true, true, true);
        assert!(all.contains(&argus_common::CAP_SYSTEMD.to_string()));
        assert!(all.contains(&argus_common::CAP_DOCKER.to_string()));
        assert!(all.contains(&argus_common::CAP_JOURNAL.to_string()));
        assert_eq!(all.len(), 3);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p argus-agent build_set_lists_only`
Expected: FAIL — `cannot find function build_set` / unresolved `argus_common::CAP_SYSTEMD`.

- [ ] **Step 3: Add the proto fields**

In `crates/proto/proto/argus.proto`, replace the `AgentInfo` message with:

```proto
message AgentInfo {
  string hostname = 1;
  string machine_id = 2;    // /etc/machine-id -- stable identity across reboots
  string os = 3;            // "Debian 12", "Flatcar 3975.2.0"
  string kernel = 4;
  string primary_ip = 5;
  string arch = 6;          // "x86_64"
  string agent_version = 7;
  // Subsystems this host actually has: "systemd" | "docker" | "journal".
  repeated string capabilities = 8;
  // True when field 8 is authoritative. proto3 cannot tell an EMPTY repeated
  // field from an ABSENT one, but a pre-capability agent (gate nothing) and a
  // capability-aware agent on a bare host (gate everything) must produce
  // opposite UI behaviour -- so the distinction is carried explicitly.
  bool capabilities_reported = 9;
}
```

- [ ] **Step 4: Add the shared name constants**

In `crates/common/src/lib.rs`, add after `CONTROL_STREAM_ID`:

```rust
/// Capability names reported by the agent on `AgentInfo` and stored in
/// `machines.capabilities`. Both binaries import these so a capability is never
/// spelled as a string literal on either side of the wire.
pub const CAP_SYSTEMD: &str = "systemd";
pub const CAP_DOCKER: &str = "docker";
pub const CAP_JOURNAL: &str = "journal";
```

- [ ] **Step 5: Implement the probes**

Put this ABOVE the test module in `crates/agent/src/capabilities.rs`:

```rust
//! What this host can actually do.
//!
//! Probed once per session, immediately before `Hello`, so the set re-reports on
//! every reconnect. Every probe is timeout-bounded and a timeout reports the
//! capability ABSENT: the agent's self-healing rests on reconnect being
//! reliable, so a capability probe must never become a new way for session open
//! to stall.

use crate::docker::DockerClient;
use crate::systemd::SystemdClient;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Per-probe ceiling. Generous enough for a busy host, short enough that all
/// three together cannot meaningfully delay a reconnect.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Assemble the reported set. Order is stable so tests and audit output do not
/// depend on iteration order.
pub fn build_set(has_systemd: bool, has_docker: bool, has_journal: bool) -> Vec<String> {
    let mut caps = Vec::new();
    if has_systemd {
        caps.push(argus_common::CAP_SYSTEMD.to_string());
    }
    if has_docker {
        caps.push(argus_common::CAP_DOCKER.to_string());
    }
    if has_journal {
        caps.push(argus_common::CAP_JOURNAL.to_string());
    }
    caps
}

/// Is `journalctl` present and runnable? `--version` answers exactly that.
///
/// It deliberately does NOT prove the journal is readable: a non-root agent gets
/// a `journalctl` that exits 0 and prints nothing, which no exit status can
/// distinguish from an empty journal. A permission failure stays a runtime
/// concern and already surfaces as a marker line in the log stream.
async fn has_journal() -> bool {
    let fut = Command::new("journalctl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(tokio::time::timeout(PROBE_TIMEOUT, fut).await, Ok(Ok(s)) if s.success())
}

/// Probe every subsystem and return the capability set for `AgentInfo`.
pub async fn probe(systemd: &SystemdClient, docker: &DockerClient) -> Vec<String> {
    // systemd is already re-dialed fresh on each reconnect attempt (zbus does
    // not self-heal), so its availability is current without another round trip.
    let systemd_ok = systemd.is_available();
    // Docker needs a REAL ping: `connect_with_socket_defaults` only constructs a
    // client and never contacts the daemon, so trusting it would claim `docker`
    // on a host where dockerd is installed but stopped.
    let docker_ok = docker.ping_ok(PROBE_TIMEOUT).await;
    let journal_ok = has_journal().await;
    build_set(systemd_ok, docker_ok, journal_ok)
}
```

Register the module in `crates/agent/src/main.rs` alongside the existing `mod` declarations:

```rust
mod capabilities;
```

- [ ] **Step 6: Add the two accessors the probe needs**

In `crates/agent/src/systemd.rs`, add to `impl SystemdClient`:

```rust
    /// Whether a system-bus connection was established for this session.
    pub fn is_available(&self) -> bool {
        self.inner.is_some()
    }
```

In `crates/agent/src/docker.rs`, add to `impl DockerClient`:

```rust
    /// Whether a daemon is actually answering, bounded by `timeout`.
    ///
    /// `connect()` succeeding only means a client was constructed — it never
    /// contacts the daemon — so this pings `/_ping` rather than trusting that.
    pub async fn ping_ok(&self, timeout: Duration) -> bool {
        let Some(docker) = &self.inner else {
            return false;
        };
        matches!(tokio::time::timeout(timeout, docker.ping()).await, Ok(Ok(_)))
    }
```

Add `use std::time::Duration;` to `docker.rs` if it is not already imported.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p argus-agent` then `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: all pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/proto/proto/argus.proto crates/common/src/lib.rs crates/agent/src/capabilities.rs crates/agent/src/main.rs crates/agent/src/systemd.rs crates/agent/src/docker.rs
git commit -m "feat(agent): probe host capabilities"
```

---

### Task 4: Report capabilities on Hello

**Files:**
- Modify: `crates/agent/src/session.rs`

**Interfaces:**
- Consumes: `capabilities::probe(&systemd, &docker)` (Task 3).

- [ ] **Step 1: Probe once per session and attach to Hello**

In `crates/agent/src/session.rs`, inside `connect_and_serve` (which already has `&docker` and `&systemd` in scope), probe **before** the sender task is spawned, then move the result into it. Immediately before `let sender = tokio::spawn(async move {`:

```rust
    // Probed once per session, not per request. Re-reporting on every reconnect
    // is what lets a host that gained (or lost) a subsystem be reflected without
    // an agent restart.
    let caps = crate::capabilities::probe(systemd, docker).await;
```

Inside the sender task, set the fields on the gathered info. Replace the `let info = match ... };` block's tail so the info is mutable and annotated:

```rust
        let mut info = match crate::info::gather(env!("CARGO_PKG_VERSION")) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "session: gathering AgentInfo for Hello failed");
                return;
            }
        };
        info.capabilities = caps;
        // Marks field 8 as authoritative. An agent that never sets this reports
        // `false`, which the control plane stores as NULL and treats as "unknown,
        // gate nothing" — the correct reading for a pre-capability agent.
        info.capabilities_reported = true;
```

`caps` must be moved into the `async move` closure — it is, because the closure already captures by move.

- [ ] **Step 2: Verify it compiles and the suite is green**

Run: `cargo test -p argus-agent` then `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: all pass, no warnings.

Note: the `Enroll` path deliberately does **not** set capabilities. A freshly enrolled machine is stored with `NULL` capabilities for the few seconds until its first `Hello`, which then populates them — "gate nothing" is the correct behaviour for that window.

- [ ] **Step 3: Add the live host test**

Add to `crates/agent/src/capabilities.rs`'s `mod tests` (the name MUST start with `live_` so CI skips it):

```rust
    /// The real probes against this machine. This host runs systemd with a
    /// readable journal, so both must be reported; docker depends on whether a
    /// daemon is answering, so only assert the call completes and is consistent
    /// with a direct ping.
    #[tokio::test]
    #[ignore = "probes the real host; run under sudo on a systemd host"]
    async fn live_probe_reports_this_hosts_capabilities() {
        let systemd = crate::systemd::SystemdClient::connect().await;
        let docker = crate::docker::DockerClient::connect();
        let caps = probe(&systemd, &docker).await;

        assert!(
            caps.contains(&argus_common::CAP_JOURNAL.to_string()),
            "journalctl is present on this host, got {caps:?}"
        );
        assert_eq!(
            caps.contains(&argus_common::CAP_DOCKER.to_string()),
            docker.ping_ok(std::time::Duration::from_secs(3)).await,
            "the reported docker capability must match a direct ping"
        );
    }
```

- [ ] **Step 4: Run the live tests**

```bash
cargo test -p argus-agent --no-run
sudo -n ./target/debug/deps/argus_agent-<hash> --ignored --test-threads=1
```
Use the binary path `--no-run` prints. All `live_*` tests must pass. **Do NOT modify group membership, sudoers, or any system configuration.**

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/session.rs crates/agent/src/capabilities.rs
git commit -m "feat(agent): report capabilities on every Hello"
```

---

### Task 5: Persist and expose capabilities

**Files:**
- Create: `crates/server/migrations/0003_capabilities.sql`
- Modify: `crates/server/src/repo.rs` (`AgentInfoRow`, `upsert_machine`, `update_machine_inventory`)
- Modify: `crates/server/src/grpc.rs` (`agent_info_row`)
- Modify: `crates/server/src/http.rs` (`MachineDetailDto` and its query)

**Interfaces:**
- Consumes: proto fields 8/9 (Task 3).
- Produces: `AgentInfoRow.capabilities: Option<Vec<String>>`; `MachineDetailDto.capabilities: Option<Vec<String>>`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/server/src/repo.rs`:

```rust
    #[sqlx::test]
    async fn capabilities_round_trip_and_none_stays_distinct_from_empty(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // Reported set survives a round trip.
        let id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-1".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec!["systemd".into(), "journal".into()]),
            },
        )
        .await?;
        let got: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(got, Some(vec!["systemd".to_string(), "journal".to_string()]));

        // An EMPTY reported set is stored as `{}` and must NOT collapse to NULL:
        // it means "this host has none", which gates everything.
        let empty_id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-2".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: Some(vec![]),
            },
        )
        .await?;
        let empty: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", empty_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(empty, Some(vec![]), "empty must stay empty, not become NULL");

        // A pre-capability agent reports nothing at all -> NULL -> gate nothing.
        let null_id = upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "caps-3".into(),
                hostname: "h".into(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;
        let null_caps: Option<Vec<String>> =
            sqlx::query_scalar!("SELECT capabilities FROM machines WHERE id = $1", null_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(null_caps, None, "unreported must stay NULL");
        Ok(())
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p argus-server capabilities_round_trip`
Expected: FAIL — `AgentInfoRow` has no field `capabilities`.

- [ ] **Step 3: Add the migration**

Create `crates/server/migrations/0003_capabilities.sql`:

```sql
-- Subsystems the agent reports this host actually has ("systemd", "docker",
-- "journal"). NULLABLE ON PURPOSE and the distinction is load-bearing:
--   NULL -> the agent never reported (predates capability reporting); the UI
--           must gate NOTHING, because absence of evidence is not evidence of
--           absence and blanking a working machine is the worst outcome here.
--   {}   -> the agent reported and this host has none; the UI gates everything.
alter table machines add column capabilities text[];
```

- [ ] **Step 4: Persist on both write paths**

In `crates/server/src/repo.rs`, add the field to `AgentInfoRow`:

```rust
pub struct AgentInfoRow {
    pub machine_id: String,
    pub hostname: String,
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub primary_ip: Option<String>,
    pub agent_version: Option<String>,
    /// `None` = the agent never reported (stored as NULL, gates nothing).
    /// `Some(vec![])` = reported and this host has none (gates everything).
    pub capabilities: Option<Vec<String>>,
}
```

In `upsert_machine`, add the column to the INSERT, the `ON CONFLICT` SET list and the bindings:

```rust
        INSERT INTO machines (machine_id, hostname, os, kernel, arch, primary_ip, agent_version, capabilities)
        VALUES ($1, $2, $3, $4, $5, $6::text::inet, $7, $8)
        ON CONFLICT (machine_id) DO UPDATE SET
            hostname      = EXCLUDED.hostname,
            os            = EXCLUDED.os,
            kernel        = EXCLUDED.kernel,
            arch          = EXCLUDED.arch,
            primary_ip    = EXCLUDED.primary_ip,
            agent_version = EXCLUDED.agent_version,
            capabilities  = coalesce(EXCLUDED.capabilities, machines.capabilities),
            updated_at    = now()
        RETURNING id
```

with `info.capabilities.as_deref()` bound as `$8`.

`coalesce` matters: an agent that reports nothing must not **erase** capabilities a previous session established. Only a positive report updates them.

Apply the same treatment in `update_machine_inventory` — add `capabilities = coalesce($8, machines.capabilities),` to the SET list and bind `info.capabilities.as_deref()` as `$8`.

- [ ] **Step 5: Carry them through the conversion**

In `crates/server/src/grpc.rs`, extend `agent_info_row`:

```rust
fn agent_info_row(info: &argus_proto::v1::AgentInfo) -> AgentInfoRow {
    AgentInfoRow {
        machine_id: info.machine_id.clone(),
        hostname: info.hostname.clone(),
        os: non_empty(&info.os),
        kernel: non_empty(&info.kernel),
        arch: non_empty(&info.arch),
        primary_ip: non_empty(&info.primary_ip),
        agent_version: non_empty(&info.agent_version),
        // Only an agent that SAYS it is reporting produces a non-NULL value.
        // proto3 decodes an absent repeated field and an empty one identically,
        // so this flag is the only thing separating "old agent, gate nothing"
        // from "bare host, gate everything".
        capabilities: info
            .capabilities_reported
            .then(|| info.capabilities.clone()),
    }
}
```

Every other `AgentInfoRow { .. }` literal in `repo.rs` and `grpc.rs` tests needs `capabilities: None` added — the compiler will point each one out.

- [ ] **Step 6: Expose it on the detail DTO**

In `crates/server/src/http.rs`, add to `MachineDetailDto`:

```rust
    /// `None` = never reported; the client must gate nothing in that case.
    capabilities: Option<Vec<String>>,
```

Add `capabilities` to that handler's `SELECT` column list and to the struct construction.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p argus-server`, then `cargo clippy -p argus-server --all-targets -- -D warnings`, then `cargo sqlx prepare --workspace -- --all-targets` and stage `.sqlx`.
Expected: all pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/server/migrations/0003_capabilities.sql crates/server/src/repo.rs crates/server/src/grpc.rs crates/server/src/http.rs .sqlx
git commit -m "feat(server): store and expose agent capabilities"
```

---

### Task 6: Gate the UI

**Files:**
- Modify: `frontend/src/api.ts` (machine detail type)
- Modify: `frontend/src/components/Tabs.tsx`
- Modify: `frontend/src/pages/MachineDetailPage.tsx`

**Interfaces:**
- Consumes: `capabilities: string[] | null` on the machine detail response (Task 5).

- [ ] **Step 1: Add the field to the machine type**

In `frontend/src/api.ts`, add to the machine-detail type (the one `getMachine` returns):

```ts
  /** `null` = the agent never reported; gate nothing. */
  capabilities: string[] | null;
```

- [ ] **Step 2: Teach Tabs about disabled tabs**

In `frontend/src/components/Tabs.tsx`, widen the props:

```tsx
export default function Tabs({
  tabs,
  active,
  onChange,
}: {
  tabs: { key: TabKey; label: string; disabled?: boolean; reason?: string }[];
  active: TabKey;
  onChange: (key: TabKey) => void;
}) {
```

Make arrow-key navigation skip disabled tabs — landing focus on a dead control breaks the ARIA pattern:

```tsx
  function onKeyDown(e: React.KeyboardEvent<HTMLDivElement>) {
    if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
    e.preventDefault();
    const i = tabs.findIndex((t) => t.key === active);
    if (i === -1) return;
    const step = e.key === "ArrowRight" ? 1 : -1;
    // Walk past disabled tabs; stop if every other tab is disabled.
    for (let n = 1; n <= tabs.length; n++) {
      const j = (i + step * n + tabs.length * tabs.length) % tabs.length;
      if (!tabs[j].disabled) {
        onChange(tabs[j].key);
        refs.current[j]?.focus();
        return;
      }
    }
  }
```

On the rendered `<button>`, add `disabled={t.disabled}`, `aria-disabled={t.disabled}`, `title={t.reason}`, and a muted style when disabled (append to the existing `cn(...)` call):

```tsx
          disabled={t.disabled}
          aria-disabled={t.disabled}
          title={t.reason}
```

- [ ] **Step 3: Derive the gating on the machine page**

In `frontend/src/pages/MachineDetailPage.tsx`, replace the static `TABS` with a derivation from the machine's capabilities.

**Ordering matters here.** Today the `?tab=` resolution sits *above* `const machineQuery = useMachine(...)`, but the derived `TABS` depends on `machineQuery.data`. Move the tab-resolution block (this step and Step 4) to **after** `machineQuery` is declared, and keep both before the `return`. Do not move `machineQuery` itself, and do not reorder any hook relative to another — `useMachine` must still be called unconditionally in the same position.

```tsx
  // `null` capabilities means the agent predates capability reporting: gate
  // NOTHING rather than blanking a working machine. An explicit (possibly
  // empty) array is authoritative.
  const caps = machineQuery.data?.capabilities ?? null;
  const lacks = (cap: string) => caps !== null && !caps.includes(cap);
  const TABS: { key: string; label: string; disabled?: boolean; reason?: string }[] = [
    { key: "overview", label: "Overview" },
    {
      key: "containers",
      label: "Containers",
      disabled: lacks("docker"),
      reason: lacks("docker") ? "no Docker daemon on this host" : undefined,
    },
    {
      key: "units",
      label: "Units",
      disabled: lacks("systemd"),
      reason: lacks("systemd") ? "no systemd on this host" : undefined,
    },
    {
      key: "logs",
      label: "Logs",
      disabled: lacks("journal"),
      reason: lacks("journal") ? "no journald on this host" : undefined,
    },
  ];
  const TAB_KEYS = TABS.map((t) => t.key);
```

Delete the old module-level `TABS`/`TAB_KEYS` constants.

- [ ] **Step 4: Extend the `?tab=` guard to disabled tabs**

Replace the tab-resolution line so a bookmarked link to a now-gated tab falls back rather than rendering a blank panel — the same forgiving behaviour `?tab=typo` already gets:

```tsx
  const requestedTab = searchParams.get("tab");
  const requested = TABS.find((t) => t.key === requestedTab);
  const tab = requested && !requested.disabled ? (requestedTab as string) : "overview";
```

Keep `TAB_KEYS` if anything else references it; otherwise remove it along with the old constants.

- [ ] **Step 5: Typecheck and build**

Run: `cd frontend && node_modules/.bin/tsc --noEmit && npm run build`
Expected: exit 0, build succeeds.

- [ ] **Step 6: Commit**

```bash
git add frontend/src/api.ts frontend/src/components/Tabs.tsx frontend/src/pages/MachineDetailPage.tsx
git commit -m "feat(frontend): disable tabs a host cannot support"
```

---

### Task 7: Verification, DEV.md, PR (controller-run)

**Files:**
- Modify: `docs/DEV.md`

- [ ] **Step 1: Full static gates**

```bash
npm --prefix frontend run build
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace -- --ignored --skip live_
```
Expected: all clean.

- [ ] **Step 2: Live host tests**

```bash
cargo test -p argus-agent --no-run
sudo -n ./target/debug/deps/argus_agent-<hash> --ignored --test-threads=1
```
Expected: all `live_*` pass.

- [ ] **Step 3: Live E2E**

With the control plane and a root agent running:
- `GET /api/machines/:id` reports `capabilities` containing `systemd`, `journal` and (if dockerd is up) `docker`.
- Stop dockerd, restart the agent, confirm `docker` disappears from the reported set — this is the check that proves `ping()` is doing real work rather than trusting client construction.
- Confirm a `meta` frame arrives first on the log stream: `curl -sN ".../logs/stream?source=journal:@system&window=1h&follow=false" | head -3` shows `event: meta` and a `since_ms`.
- Confirm `logs/page` with an explicit `since_ms` returns only entries at or after it, and that a bad `since_ms` is a `400`.
- Set `capabilities = NULL` for the machine directly in Postgres and confirm every tab is enabled again (the pre-capability agent path).

- [ ] **Step 4: Browser pass**

- On this (systemd + journal) host every tab is enabled.
- With `capabilities` forced to `{}` in the DB, Containers/Units/Logs are disabled and state a reason; Overview still works.
- Arrow-key navigation across the tab strip skips disabled tabs.
- A bookmarked `?tab=units` on a gated machine lands on Overview rather than a blank panel.

- [ ] **Step 5: Record in DEV.md and commit**

Add a "Capability reporting + log-window fix" section to `docs/DEV.md` covering the checks above, the tri-state table, and the `coalesce` rule (a silent agent never erases known capabilities).

```bash
git add docs/DEV.md
git commit -m "docs: record capability reporting verification"
```

- [ ] **Step 6: Open the PR**

Write the body to a file, then:

```bash
fj pr create "feat(agent): report host capabilities and gate unsupported surfaces" \
  --base main --head capability-reporting-slice --body-file <path>
```
