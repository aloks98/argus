# Argus — Product Requirements Document

> Argus — the hundred-eyed watchman. Centralized fleet management for a
> multi-Proxmox-host homelab. "Cockpit, but centralized": one control plane for
> observability, terminal access, and lifecycle operations across many VMs and
> LXCs, without SSH-ing into each guest.

**Status:** V1 design, pre-implementation.
**Language:** Rust (control plane + agent). Peer project **Komodo** (also Rust) is
useful reference material.

---

## 1. Purpose & scope

### What Argus is
A single control plane that gives you one screen for the whole fleet: live
metrics, health badges, container/service state and control, on-demand log
tailing, and an interactive terminal — reachable from a browser, backed by a thin
agent on every guest.

### What Argus is NOT
**Argus is not a deployment platform.** Deploys stay in the existing workflow
(docker build / binaries per VM). Argus is for *seeing and operating* the fleet.
Any earlier framing about rendering cloud-init specs or immutable rebuilds is
**discarded**. Cloud-init remains provisioning-only (first boot + base layer); the
agent's systemd unit is simply added to the existing per-distro base (zsh, MOTD,
SSH hardening, apt proxy at `192.168.150.1:3142`), which is otherwise untouched.

### Goals (V1)
- One agent enrolls, connects over mTLS, heartbeats, and appears on a fleet page.
- Host metrics + rolling history with sparklines.
- Docker & systemd state, plus a short list of control verbs.
- On-demand log tailing (journal + docker).
- Interactive terminal.
- ntfy alerts, Zitadel OIDC, audit log of every verb.

### Non-goals (V1)
- Script library / bulk execution, file browser, updates dashboard, Proxmox
  correlation, maintenance windows → **V1.1**.
- Scheduled tasks, osquery-style fleet queries, Prometheus re-export, WoL, session
  recording, RBAC → **V2**.
- Continuous log shipping (logs are *pull-on-demand* only, forever).
- Kubernetes manifests are an **appendix deliverable**, generated last once the app
  shape is stable — not maintained in parallel from day one.

---

## 2. Architecture

### 2.1 Components
- **Control plane** — a single Rust binary with the React frontend embedded via
  `rust-embed`. Stateless (all state in Postgres) so the pod can reschedule
  freely. Serves two network surfaces (see §2.4).
- **Agent** — a thin Rust binary on every guest. Dials **outbound** to the control
  plane and holds **one persistent connection** over which everything is
  multiplexed. No inbound firewall rules on any guest; NAT/VLAN-agnostic; identical
  behavior in LXC and Flatcar.
- **Postgres** — external (CloudNativePG, barman → S3). The only stateful piece.

### 2.2 Transport
**gRPC bidirectional stream** via `tonic`, chosen over embedded NATS for V1
simplicity: one proto, application-level **stream-ID multiplexing**, no extra
moving parts. Metrics, docker/systemd state, log tails, PTY, and command results
all ride the one `Session` stream, tagged by `stream_id`. NATS remains a later
option only if multi-replica or cross-service fan-out is ever required.

### 2.3 Auth
**mTLS**, with the control plane running its **own internal CA** (`rcgen` for cert
issuance, `rustls` for the TLS listener). Each agent is issued a client cert on
enrollment; no external CA dependency. The CA private key is persisted in Postgres,
**AES-256-GCM field-encrypted** with a key supplied via K8s Secret → env — this is
what lets a stateless pod reschedule without losing the identity root the entire
fleet trusts. See §5.

### 2.4 Two entrypoints (do not reopen)
The control plane exposes **two deliberately separate network surfaces**:

| Surface | Who | Path in | TLS |
|---|---|---|---|
| **Browser** | humans | Traefik IngressRoute + cert-manager + Zitadel OIDC | Traefik-terminated, public CA. Host `argus.lab.<domain>` |
| **Agents** | agents | dedicated `LoadBalancer` Service, pinned MetalLB IP | **control plane terminates mTLS itself.** Host `agents.argus.lab.<domain>` |

**gRPC agent traffic must NOT route through Traefik** — proxying it would break
end-to-end client-certificate verification. The agent LoadBalancer hands raw TCP to
the pod, which runs its own `rustls` mTLS listener. Both DNS records via
PowerDNS/DNSaur; the agent endpoint + Argus CA cert are baked into the
Ignition/cloud-init base. **Collapsing these two entrypoints is a decision that
must not be reopened.**

### 2.5 Design for pod churn
Single replica, `strategy: Recreate` (avoid split-brain agent connections during
rollout). Agents therefore experience the control plane blinking out on every
deploy. Handling:
- Agents reconnect with **exponential backoff + jitter** (avoid a stampede from
  ~40 guests).
- On reconnect the agent **re-sends a full state snapshot** (`Hello`) so the fleet
  view self-heals.
- UI shows **"reconnecting"** per machine rather than an empty fleet.
- Terminal sessions drop on redeploy (acceptable — same as SSH).
- Readiness probe is gated on Postgres connectivity so the pod does not accept
  agents before migrations finish.

### 2.6 Known circular dependency (accepted)
Argus runs on K8s → on Flatcar VMs → on the Proxmox hosts it watches. If the
cluster is down, so is Argus. Mitigated by (1) Proxmox-correlation verbs going
through the PVE API (work even when everything above the hypervisor is down —
V1.1), and (2) the existing kube-prometheus-stack covering the "cluster itself is
sick" case from outside Argus. On-cluster was chosen over a standalone VM to keep
CNPG / cert-manager / GitOps leverage.

---

## 3. Repository layout

Single Cargo **workspace** (the faithful Rust translation of the original "single
Go module"). One lockfile; crates split by role:

```
argus/
├─ Cargo.toml                  # workspace: members, shared [workspace.dependencies]
├─ rust-toolchain.toml         # pins toolchain + records the musl release target
├─ crates/
│  ├─ proto/                   # argus-proto: the gRPC contract
│  │  ├─ build.rs              #   protox (pure-Rust) -> tonic-prost-build codegen
│  │  ├─ proto/argus.proto     #   THE source of truth for the agent protocol
│  │  └─ src/lib.rs            #   re-exports generated argus.v1 module
│  ├─ common/                  # argus-common: tiny shared constants/types
│  ├─ server/                  # argus-server: the control-plane binary `argus`
│  │  ├─ migrations/           #   sqlx embedded migrations (run on startup)
│  │  └─ src/                  #   main + config/db/ca/grpc/http/jobs modules
│  └─ agent/                   # argus-agent: the guest binary `argus-agent`
└─ frontend/                   # Vite + React + TS, @e412/rnui-react; built to
   └─ dist/                    #   dist/ and embedded into `argus` via rust-embed
```

Binaries: control plane = `argus` (from `crates/server`); agent = `argus-agent`
(from `crates/agent`).

---

## 4. The agent gRPC protocol (concrete)

`crates/proto/proto/argus.proto` is the source of truth. The full V1 contract:

```proto
syntax = "proto3";
package argus.v1;

// Served on the MetalLB agent LoadBalancer (agents.argus.lab.<domain>). The
// control plane terminates TLS itself with rustls configured for OPTIONAL client
// auth. Per-RPC enforcement:
//   - Enroll : NO client cert required; gated by a join token. Returns the
//              agent's signed client certificate (its mTLS identity).
//   - Session: client cert REQUIRED and validated against the internal CA; the
//              agent_id is read from the cert. One long-lived bidi stream carries
//              everything, multiplexed by stream_id.
service AgentService {
  rpc Enroll (EnrollRequest) returns (EnrollResponse);
  rpc Session (stream AgentFrame) returns (stream ServerFrame);
}

// ---- Enrollment ----------------------------------------------------------

message EnrollRequest {
  string    join_token = 1;   // shared secret baked into the template
  string    csr_pem    = 2;   // agent-generated CSR; the private key never leaves the agent
  AgentInfo info       = 3;
}

message EnrollResponse {
  string client_cert_pem = 1; // signed by the internal CA
  string ca_cert_pem     = 2; // so the agent can also verify the server
  string agent_id        = 3; // UUID assigned to this machine
}

message AgentInfo {
  string hostname      = 1;
  string machine_id    = 2;   // /etc/machine-id — stable identity across reboots
  string os            = 3;   // "Debian 12", "Flatcar 3975.2.0"
  string kernel        = 4;
  string primary_ip    = 5;
  string arch          = 6;   // "x86_64"
  string agent_version = 7;
}

// ---- Session multiplexing ------------------------------------------------
// stream_id == 0  -> connection-level control (hello/heartbeat/ping).
// stream_id  > 0  -> a logical sub-stream (one command, one log tail, one PTY).

message AgentFrame {
  uint64 stream_id = 1;
  oneof payload {
    Hello         hello          = 2;   // first frame after connect; snapshot begins
    Heartbeat     heartbeat      = 3;
    MetricsSample metrics        = 4;
    DockerState   docker_state   = 5;
    SystemdState  systemd_state  = 6;
    LogChunk      log_chunk      = 7;
    PtyOutput     pty_output     = 8;
    CommandResult command_result = 9;
    Ack           ack            = 10;
  }
}

message ServerFrame {
  uint64 stream_id = 1;
  oneof payload {
    HelloAck       hello_ack      = 2;
    Command        command        = 3;   // a verb to execute
    LogTailRequest log_tail_start = 4;
    LogTailStop    log_tail_stop  = 5;
    PtyOpen        pty_open       = 6;
    PtyInput       pty_input      = 7;
    PtyResize      pty_resize     = 8;
    PtyClose       pty_close      = 9;
    UpdateAgent    update         = 10;  // self-update: download new binary + re-exec
    Ping           ping           = 11;
  }
}

// ---- Connection control --------------------------------------------------

message Hello     { AgentInfo info = 1; }         // full snapshot follows
message HelloAck  { string server_version = 1; uint32 heartbeat_secs = 2; }
message Heartbeat { int64 unix_ms = 1; uint64 uptime_secs = 2; }
message Ping      { int64 unix_ms = 1; }
message Ack       { uint64 ref_stream_id = 1; bool ok = 2; string message = 3; }

// ---- Metrics -------------------------------------------------------------

message MetricsSample {
  int64  unix_ms      = 1;
  float  cpu_pct      = 2;
  uint64 mem_used     = 3;
  uint64 mem_total    = 4;
  uint64 swap_used    = 5;
  uint64 swap_total   = 6;
  float  load1        = 7;
  float  load5        = 8;
  float  load15       = 9;
  uint64 disk_used    = 10;  // aggregate of monitored filesystems
  uint64 disk_total   = 11;
  uint64 net_rx_bytes = 12;  // cumulative; deltas computed control-plane-side
  uint64 net_tx_bytes = 13;
  uint64 uptime_secs  = 14;
  // Long tail (per-disk, per-net, temps, ZFS ARC) rides a JSON string so the
  // proto and the metrics table both stay boring.
  string extra_json   = 15;
}

// ---- Docker & systemd state ---------------------------------------------

message DockerState { repeated Container containers = 1; }
message Container {
  string id     = 1;
  string name   = 2;
  string image  = 3;
  string state  = 4;   // running|exited|paused|...
  string status = 5;   // human string
  string health = 6;   // healthy|unhealthy|starting|"" (none)
}

message SystemdState { repeated Unit units = 1; }
message Unit {
  string name         = 1;
  string load_state   = 2;   // loaded|not-found|...
  string active_state = 3;   // active|failed|inactive|...
  string sub_state    = 4;   // running|dead|exited|...
  string description   = 5;
}

// ---- Verbs (V1: deliberately short) -------------------------------------

enum Verb {
  VERB_UNSPECIFIED   = 0;
  CONTAINER_START    = 1;
  CONTAINER_STOP     = 2;
  CONTAINER_RESTART  = 3;
  UNIT_START         = 4;
  UNIT_STOP          = 5;
  UNIT_RESTART       = 6;
}

message Command {
  string command_id = 1;   // UUID; correlates to audit_log + CommandResult
  Verb   verb       = 2;
  string target     = 3;   // container id or unit name
  string issued_by  = 4;   // OIDC subject/email — for the agent-side audit trail
}

message CommandResult {
  string command_id = 1;
  bool   ok         = 2;
  int32  exit_code  = 3;
  string message    = 4;
}

// ---- Log tailing (pull-on-demand) ---------------------------------------

message LogTailRequest {
  string request_id = 1;
  string source     = 2;   // "docker:<container-id>" | "journal:<unit>"
  uint32 tail_lines = 3;
  bool   follow     = 4;
}
message LogTailStop { string request_id = 1; }
message LogChunk {
  string request_id = 1;
  bytes  data       = 2;
  bool   eof        = 3;
}

// ---- Terminal (PTY) ------------------------------------------------------

message PtyOpen   { string session_id = 1; uint32 cols = 2; uint32 rows = 3; string shell = 4; }
message PtyInput  { string session_id = 1; bytes  data = 2; }
message PtyOutput { string session_id = 1; bytes  data = 2; }
message PtyResize { string session_id = 1; uint32 cols = 2; uint32 rows = 3; }
message PtyClose  { string session_id = 1; }

// ---- Self-update ---------------------------------------------------------

message UpdateAgent {
  string url     = 1;   // mTLS URL on the control plane to fetch the new binary
  string version = 2;
  string sha256  = 3;   // verified before re-exec
}
```

### Multiplexing rules
- `stream_id == 0` carries `Hello`/`Heartbeat`/`Ping`/`Ack`.
- A `Command`, a `LogTailRequest`, or a `PtyOpen` establishes a sub-stream; the
  server picks a fresh non-zero `stream_id`, and all frames for that operation
  (its `CommandResult`, `LogChunk`s, `PtyInput`/`PtyOutput`) share it.
- The agent maps each active `stream_id` to a local task (a running command, an
  open journal follower, a PTY). `LogTailStop`/`PtyClose` tears it down.

### Self-update
`UpdateAgent` names a binary URL on the control plane's **agent (mTLS)** surface,
a version, and a sha256. The agent downloads over its authenticated channel,
verifies the hash, swaps the binary, and re-exec's. (Wired in the protocol now;
scheduled for late V1 / V1.1.)

---

## 5. Enrollment & mTLS handshake (highest-risk component — build & verify first)

> Every downstream feature (terminal, verbs, log tail) trusts that a `Session` is
> authenticated and correctly identified. This slice is built and **manually
> verified with a real agent before anything else layers on top.** It is build
> slice #1 (§8).

### 5.1 Provisioning (build time)
1. Operator creates an enrollment token in the Argus UI. The server stores
   **`sha256(token)`** in `enrollment_tokens` and displays the raw token **once**.
2. The template bakes in: the agent binary, its systemd unit, the join token, the
   agent endpoint (`agents.argus.lab.<domain>`), and the **Argus CA certificate**
   (public, not secret — needed to verify the server on the very first call).

### 5.2 First boot (agent)
3. Agent reads `join_token`, `ca_cert`, and `endpoint` from its config
   (env/file). It looks for an existing client cert at
   `/var/lib/argus/agent.crt`+`.key`. If present and valid → skip to §5.4.
4. Agent generates a keypair locally (via `rcgen`) and builds a CSR (CN =
   machine-id). **The private key is written to `/var/lib/argus/agent.key` (0600)
   and never transmitted.**
5. Agent opens **server-authenticated TLS** (verifying the server against the baked
   CA cert) and calls `Enroll { join_token, csr_pem, info }`.

### 5.3 Server (Enroll handler)
6. Hash the presented token; look up `enrollment_tokens` by hash. Reject if
   missing / revoked / expired / uses exhausted.
7. Load CA material: decrypt the CA private key (AES-256-GCM, key from env) →
   parse the CSR → **sign a client certificate** whose subject encodes the assigned
   `agent_id`; validity ~1 year. Increment the token's `uses`.
8. Upsert `machines` by `machine_id` (→ get/create `agent_id`); insert an
   `agent_certs` row (serial, fingerprint, validity window).
9. Return `EnrollResponse { client_cert_pem, ca_cert_pem, agent_id }`. Write
   `audit_log` (`action = agent.enroll`, `actor = <token name>`).

### 5.4 Session (mTLS, persistent)
10. Agent writes the client cert to disk, then opens a **mutually-authenticated
    TLS** connection (presents its client cert; verifies the server via CA) and
    calls `Session()`.
11. The rustls listener runs with **optional client auth**. A tonic interceptor
    enforces per-RPC policy: `Enroll` is the only RPC permitted without a client
    cert; every other RPC requires a validated cert. For `Session`, the interceptor
    extracts the `agent_id` from the peer cert, confirms `agent_certs.revoked =
    false` and not expired, and rejects otherwise.
12. Agent sends `AgentFrame { hello }` + a full snapshot (current docker/systemd
    state). Server marks the machine `online`, updates `last_seen_at`, binds the
    connection to `agent_id`.
13. Steady state: agent streams `Heartbeat` + `MetricsSample` (~15s); server sends
    `Command` / `LogTailRequest` / `PtyOpen` down the same stream. All keyed by
    `stream_id`.
14. On disconnect: server marks `offline` after a missed-heartbeat grace period.
    Agent reconnects with exponential backoff + jitter and re-sends `Hello` (full
    snapshot) so the fleet view self-heals.

### 5.5 Sequence diagram
```
  Agent (first boot)                         Control plane (agent LB, mTLS)
  ─────────────────                          ──────────────────────────────
  gen keypair + CSR (rcgen)
  server-auth TLS (verify via baked CA) ───▶
  Enroll{token, csr, info} ───────────────▶  validate token
                                             sign cert from CSR (CA key, decrypted)
                                             upsert machines / insert agent_certs
                                             audit: agent.enroll
  ◀─────────────── EnrollResponse{cert,ca,id}
  persist cert to disk
  ── close, reopen ──
  mTLS TLS (present client cert) ─────────▶  optional-client-auth listener
  Session() open ────────────────────────▶  interceptor: validate cert -> agent_id
  AgentFrame{hello + snapshot} ──────────▶  mark online; bind conn to agent_id
  Heartbeat/Metrics (~15s) ──────────────▶  persist; recompute health badges
                       ◀──────────────────  Command / LogTail / PtyOpen (stream_id)
```

### 5.6 Cert lifecycle
Certs are ~1-year. The agent watches `not_after`; before expiry it re-runs the
enroll flow (token still valid) to obtain a fresh cert. Revocation is a flag on
`agent_certs` checked at `Session` admission (no CRL/OCSP needed at this scale). A
dedicated `Renew` RPC is a V1.1 refinement.

---

## 6. Data model & migrations

`sqlx` with **compile-time-checked queries** against this schema. Migrations are
**embedded** (`sqlx::migrate!`) and **run on startup** — no init container.
Files live in `crates/server/migrations/`.

### 6.1 `0001_init.sql`
```sql
-- machines: identity + inventory + agent connectivity + org + (nullable) PVE map
create table machines (
    id            uuid primary key default gen_random_uuid(),
    machine_id    text not null unique,           -- /etc/machine-id
    hostname      text not null,
    os            text,
    kernel        text,
    arch          text,
    primary_ip    inet,
    agent_version text,
    status        text not null default 'pending', -- pending|online|offline
    last_seen_at  timestamptz,
    enrolled_at   timestamptz not null default now(),
    pve_node      text,                            -- Proxmox correlation (V1.1)
    pve_vmid      integer,
    tags          text[] not null default '{}',
    notes         text,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index machines_status_idx on machines (status);
create index machines_tags_idx   on machines using gin (tags);

-- enrollment_tokens: join tokens. The raw token is NEVER stored (only sha256).
create table enrollment_tokens (
    id         uuid primary key default gen_random_uuid(),
    name       text not null,
    token_hash bytea not null unique,
    max_uses   integer,                            -- null = unlimited
    uses       integer not null default 0,
    expires_at timestamptz,                        -- null = never
    revoked    boolean not null default false,
    created_by text,
    created_at timestamptz not null default now()
);

-- agent_certs: issued client certs -> mTLS identity + revocation
create table agent_certs (
    id          uuid primary key default gen_random_uuid(),
    machine_id  uuid not null references machines(id) on delete cascade,
    serial      numeric not null unique,           -- x509 serial
    fingerprint text not null unique,              -- sha256(DER), hex
    not_before  timestamptz not null,
    not_after   timestamptz not null,
    revoked     boolean not null default false,
    revoked_at  timestamptz,
    created_at  timestamptz not null default now()
);
create index agent_certs_machine_idx on agent_certs (machine_id);

-- ca_material: singleton internal-CA root. Private key is AES-256-GCM encrypted
-- with the field key from env. Persisting it here is what lets the stateless pod
-- reschedule without losing the fleet's identity root.
create table ca_material (
    id             integer primary key default 1 check (id = 1),
    cert_pem       text  not null,
    key_ciphertext bytea not null,
    key_nonce      bytea not null,
    created_at     timestamptz not null default now()
);

-- audit_log: every verb, from day one. Never bolted on later.
create table audit_log (
    id         bigint generated always as identity primary key,
    ts         timestamptz not null default now(),
    actor      text not null,                       -- OIDC subject/email, or 'system'
    action     text not null,                       -- container.restart, unit.stop, terminal.open, agent.enroll, ...
    machine_id uuid references machines(id) on delete set null,
    target_ref text,                                -- container id / unit name
    command_id uuid,                                -- correlate to the gRPC Command
    result     text,                                -- ok|error|denied
    detail     jsonb not null default '{}'
);
create index audit_log_ts_idx      on audit_log (ts);
create index audit_log_machine_idx on audit_log (machine_id, ts);
```

### 6.2 `0002_metrics.sql`
```sql
-- The deliberately-boring metrics table (do not reopen): plain rows, BRIN on ts,
-- nightly prune. No Timescale/partitioning until a MEASURED problem forces it.
create table metrics (
    machine_id   uuid not null references machines(id) on delete cascade,
    ts           timestamptz not null,
    cpu_pct      real,
    mem_used     bigint,
    mem_total    bigint,
    swap_used    bigint,
    swap_total   bigint,
    load1        real,
    load5        real,
    load15       real,
    disk_used    bigint,
    disk_total   bigint,
    net_rx_bytes bigint,   -- cumulative counters; deltas computed at read time
    net_tx_bytes bigint,
    uptime_secs  bigint,
    extra        jsonb not null default '{}'   -- per-disk, per-net, temps, ZFS ARC
);
-- BRIN is the whole point: a tiny index over an append-only time column.
create index metrics_ts_brin      on metrics using brin (ts) with (pages_per_range = 32);
create index metrics_machine_ts   on metrics (machine_id, ts desc);
```

### 6.3 Retention
Nightly `delete from metrics where ts < now() - interval '48 hours'`. V1 runs this
from a `tokio` interval; it moves to an **apalis cron job** once apalis is wired
(§7) so it survives restarts on a schedule.

### 6.4 Later tables (not created in V1)
Script library + runs, scheduled tasks, alert config, maintenance windows arrive
with their features (V1.1/V2). apalis owns job/run **execution** state in its own
tables. Tags are a `text[]` column on `machines` for V1 (no join table yet).

---

## 7. Background work

Two mechanisms, one clean rule, **no third**:

- **`apalis` on Postgres** — anything that must survive a pod restart: script runs,
  scheduled tasks, the nightly metrics prune. A job is a struct; a worker consumes
  it; retries + cron are built in. Reuses the Postgres already backed up by barman —
  no Redis, no separate server process.
- **`tokio::spawn`** — trivial fire-and-forget where loss on restart is fine (e.g.
  a one-off container-restart verb dispatch).

**Rule (also in CLAUDE.md):** *survives-restart / retried / scheduled → apalis;
trivial and loss-tolerant → tokio task.*

**Build-time validation gate (first implementation task for any job feature):**
confirm the current `apalis-sql` **Postgres** backend is on a recent release and
that **retries + cron work on the Postgres backend specifically** (not only Redis),
and that its `sqlx` version lines up with ours. **If it disappoints, fall back to
`pgmq`** (SQS-style queue on a Postgres table + a hand-written worker loop — more
manual, hard to get wrong). Only drop to pgmq if apalis disappoints. Because of
this gate, apalis is **not** a compiled dependency in the skeleton — the V1 "Spine"
slice needs no job queue.

> Restate was considered and dropped (durable-execution engine, heavier than
> needed; its K/V has a 24h retention default so it can't be system-of-record). If
> genuine durable multi-machine exactly-once fan-out is ever needed, the cluster
> already runs Restate — but nothing in Argus requires it.

---

## 8. Phasing & build order

### Phasing
- **V1 — centralized Cockpit core:** agent + enrollment, fleet dashboard, metrics +
  history, docker/systemd state + verbs, log tailing, terminal, ntfy alerts,
  Zitadel OIDC, audit log.
- **V1.1:** script library (tag-targeted bulk exec), file browser, updates
  dashboard, Proxmox correlation, maintenance windows.
- **V2:** scheduled tasks, osquery-style fleet queries, Prometheus re-export, WoL,
  session recording, RBAC.

### Build order — thin vertical slices, not layers
Each slice is end-to-end and independently testable (one slice ≈ one orchestrated
unit of work):
1. **Spine** — one agent enrolls → connects over mTLS → heartbeats → appears on a
   fleet page. **Prove the hard part before building on it. Verify manually.**
2. Metrics + sparklines (`sysinfo`).
3. Docker state + container verbs (`bollard`).
4. Systemd state + unit verbs (`zbus`).
5. Log tailing (journal + docker, pull-on-demand).
6. Terminal (`portable-pty` + xterm.js over WebSocket).

Per-slice dependencies are added when the slice is built (the agent stays lean):
`sysinfo` at slice 2, `bollard` at 3, `zbus` at 4, `portable-pty` at 6.

---

## 9. V1 API surface

### 9.1 Browser surface (HTTPS via Traefik; Zitadel OIDC)
Live one-way streams use **SSE**; the bidirectional terminal uses **WebSocket**.

**Auth**
- `GET /auth/login` → redirect to Zitadel (authorization-code flow)
- `GET /auth/callback` → exchange code, set session cookie
- `GET /api/me` → current identity

**Fleet & machines**
- `GET  /api/fleet` → machines with status, latest metric summary, health badges
- `GET  /api/machines/:id` → detail (inventory, status, PVE map, tags, notes)
- `PATCH /api/machines/:id` → update tags / notes
- `GET  /api/machines/:id/metrics?range=1h` → history for sparklines
- `GET  /api/machines/:id/metrics/stream` *(SSE)* → live samples
- `GET  /api/machines/:id/docker` → current container list
- `GET  /api/machines/:id/systemd` → current unit list

**Verbs** (each enqueues a `Command` down the agent stream; each is audit-logged
and returns a `command_id`)
- `POST /api/machines/:id/docker/:container/:action` — `action ∈ start|stop|restart`
- `POST /api/machines/:id/units/:unit/:action` — `action ∈ start|stop|restart`

**Logs & terminal** (pull-on-demand — the tail opens on the agent for the life of
the connection and closes when the client disconnects)
- `GET /api/machines/:id/logs/stream?source=docker:<id>|journal:<unit>&tail=200`
  *(SSE)*
- `GET /api/machines/:id/terminal` *(WebSocket)* → PTY, proxied to xterm.js

**Events & audit & enrollment**
- `GET    /api/events` *(SSE)* → fleet-wide events (online/offline, command results)
  so the UI updates live and shows "reconnecting"
- `GET    /api/audit?machine_id=&actor=&limit=` → audit query
- `GET    /api/enroll-tokens` / `POST /api/enroll-tokens` (returns raw token once) /
  `DELETE /api/enroll-tokens/:id` (revoke)

**Infra (not behind OIDC)**
- `GET /healthz` → liveness (HTTP-only)
- `GET /readyz` → readiness (checks Postgres; gates agent acceptance)
- `GET /metrics` → Prometheus re-export (path reserved; V2)

**Static**
- `GET /*` → the embedded React app (rust-embed) with SPA fallback to `index.html`

### 9.2 Agent surface (mTLS on the MetalLB LoadBalancer — never via Traefik)
- gRPC `argus.v1.AgentService/Enroll`
- gRPC `argus.v1.AgentService/Session`
- `GET /agent/binary/:arch/:version` → self-update download (mTLS; reserved)

---

## 10. Frontend

Vite + React + TypeScript, component library **`@e412/rnui-react`** (Rnui — *not*
the older Titanium library). Built to `frontend/dist/` and embedded into the
`argus` binary via `rust-embed`; the HTTP layer serves it with SPA fallback. Live
data via the SSE/WebSocket endpoints in §9. `xterm.js` drives the terminal.

The skeleton ships a **placeholder page** with the full build+embed pipeline wired;
real screens (fleet dashboard, machine detail, terminal) are built with their
slices. `@e412/rnui-react` is declared as a dependency now and introduced into the
UI during implementation.

---

## 11. Security notes
- The CA private key exists in plaintext only in memory; at rest it is AES-256-GCM
  encrypted in `ca_material` with a key injected via K8s Secret → env.
- Enrollment join tokens are stored only as `sha256`; the raw token is shown once.
- Agent private keys never leave the guest (CSR flow).
- Every verb writes `audit_log` with the OIDC identity — from day one, not later.
- Proxmox API token + AES field key are K8s Secrets → env vars.
- Client-cert admission is enforced per-RPC by an interceptor (optional client auth
  at the TLS layer; hard requirement at the app layer for everything but `Enroll`).

---

## 12. Decisions that must NOT be reopened
1. **Two-entrypoint mTLS split.** Traefik for the browser; a dedicated MetalLB
   LoadBalancer for agent gRPC with the control plane terminating mTLS itself.
   Collapsing them (routing gRPC through Traefik) breaks end-to-end client-cert
   verification.
2. **The boring metrics table.** Plain table + BRIN(ts) + nightly prune. No
   Timescale / partitioning until a *measured* problem forces it.

(Plus the standing conventions in CLAUDE.md: static musl agent; every verb
audit-logged from the start; embedded migrations run on startup; UI is Rnui;
apalis-vs-tokio background rule; k8s manifests generated last.)

---

## 13. Appendix — Kubernetes topology (generated last, not part of V1 skeleton)
- Control plane pod **stateless** (DB external) → single replica, `strategy:
  Recreate`.
- Browser: Traefik IngressRoute + cert-manager TLS + Zitadel OIDC,
  `argus.lab.<domain>`.
- Agents: dedicated `LoadBalancer` Service, pinned MetalLB IP,
  `agents.argus.lab.<domain>`; mTLS terminated by the pod.
- Secrets → env: Proxmox API token, AES-256-GCM field key.
- Probes: liveness on HTTP; readiness gated on Postgres.
- Image: multi-stage build (`cargo-chef` for layer caching) → GHCR / local
  registry. The agent is built for `x86_64-unknown-linux-musl` (fully static, runs
  on Flatcar).
- Postgres: CloudNativePG, barman → S3.

Manifests are authored once the app shape is stable — deliberately not maintained
in parallel from day one.
