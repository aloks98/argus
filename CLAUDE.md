# CLAUDE.md — Argus

Centralized fleet management for a multi-Proxmox-host homelab. "Cockpit, but
centralized": one control plane for observability, terminal, and lifecycle ops
across many VMs/LXCs, without SSH-ing into each guest. **Argus is not a deployment
platform** — it *sees and operates* the fleet.

Full design: **`docs/PRD.md`**. Read it before implementing a slice.

## Stack
- **Language:** Rust. Single Cargo **workspace**. Peer project **Komodo** (Rust) is
  reference material.
- **Control plane** (`crates/server`, binary `argus`): stateless; state in Postgres.
  `axum` HTTP + `tonic` gRPC; React frontend embedded via `rust-embed`.
- **Agent** (`crates/agent`, binary `argus-agent`): thin, read-mostly. Dials
  outbound; holds one persistent mTLS `Session` stream; everything multiplexed by
  `stream_id`.
- **Transport:** `tonic` gRPC bidi stream. **Proto is the source of truth:**
  `crates/proto/proto/argus.proto` (compiled protoc-free via `protox` +
  `tonic-prost-build`; no `protoc` needed).
- **Auth:** mTLS with an internal CA (`rcgen` issues certs; `rustls` listener).
- **DB:** Postgres via `sqlx` (compile-time-checked queries). Migrations embedded
  (`sqlx::migrate!`), run on startup.
- **Frontend:** Vite + React + TS, **`@e412/rnui-react`** (Rnui) on Tailwind v4.
- **Crypto provider:** `ring` everywhere (rustls / tokio-rustls / rcgen / sqlx). Do
  not pull `aws-lc-rs` — it needs cmake and buys us nothing here.

## Repository layout
```
crates/proto    argus-proto   — the gRPC contract + build.rs codegen
crates/common   argus-common  — constants + small shared types
crates/server   argus-server  — control plane (bin: argus); migrations/ live here
crates/agent    argus-agent   — guest agent (bin: argus-agent)
frontend/       Vite React app — builds to dist/, embedded into argus
docs/PRD.md     the design of record
```

## Build & run
```bash
# Frontend must be built BEFORE the server (rust-embed embeds frontend/dist):
npm --prefix frontend ci
npm --prefix frontend run build

cargo check --workspace         # or: cargo build --release
cargo run -p argus-server        # needs Postgres; see env vars below

# Agent release build is static for Flatcar:
rustup target add x86_64-unknown-linux-musl
cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
```
Env (see `argus_common::env`): control plane needs `ARGUS_DATABASE_URL`,
`ARGUS_FIELD_KEY` (base64 AES-256-GCM); optional `ARGUS_HTTP_ADDR`,
`ARGUS_AGENT_ADDR`. Agent needs `ARGUS_AGENT_ENDPOINT`, `ARGUS_JOIN_TOKEN`,
`ARGUS_CA_CERT`.

## Standing conventions
- **Agent builds static via `x86_64-unknown-linux-musl`** (must run on Flatcar).
  Keep the agent lean: add per-slice deps only when their slice is built
  (`sysinfo`→metrics, `bollard`→docker, `zbus`→systemd, `portable-pty`→terminal).
- **Every verb goes through the audit log from the start** — not bolted on later.
  A verb without an `audit_log` write is incomplete.
- **Migrations are embedded and run on startup.** No init container.
- **UI uses Rnui (`@e412/rnui-react`)** — not the older Titanium library.
- **k8s manifests are generated last**, once the app shape is stable — an appendix
  deliverable, not maintained in parallel from day one.

## Background-work rule (there is no third mechanism)
- **survives-restart / retried / scheduled → `apalis` on Postgres** (script runs,
  scheduled tasks, nightly metrics prune).
- **trivial / loss-tolerant → `tokio::spawn`** (e.g. a one-off verb dispatch).
- **Before wiring any apalis job**, pass the build-time validation gate: confirm
  `apalis-sql`'s **Postgres** backend supports retries + cron (not only Redis) on a
  recent release and its `sqlx` version aligns with ours. If it disappoints, fall
  back to **`pgmq`**. This is why apalis is not yet a compiled dependency.

## Decisions that must NOT be reopened
1. **Two-entrypoint mTLS split.** Traefik (+ cert-manager + Zitadel OIDC) for the
   browser; a dedicated **MetalLB LoadBalancer** for agent gRPC, with the control
   plane terminating **mTLS itself**. Routing gRPC through Traefik breaks
   end-to-end client-cert verification. Never collapse these.
2. **The boring metrics table.** Plain table + `BRIN(ts)` + nightly `DELETE`. No
   Timescale / partitioning until a *measured* problem forces it.

## Build order — thin vertical slices (not layers)
Each slice is end-to-end and independently testable (≈ one orchestrated unit).
1. **Spine** — one agent enrolls → connects over mTLS → heartbeats → shows on a
   fleet page. **Highest-risk; build and manually verify FIRST.** The whole system
   trusts that a `Session` is authenticated and identified (PRD §5).
2. Metrics + sparklines (`sysinfo`).
3. Docker state + container verbs (`bollard`).
4. Systemd state + unit verbs (`zbus`).
5. Log tailing (journal + docker, pull-on-demand).
6. Terminal (`portable-pty` + xterm.js over WebSocket).

## Skeleton status (pre-implementation)
Structure, proto, schema, and the frontend embed pipeline are in place and compile.
Subsystems marked `todo!()` / `#![allow(dead_code)]` (CA, agent gRPC serve,
enrollment, session loop, jobs) are stubs carrying their intended shape; they are
filled in starting with the Spine slice. The browser surface (health + embedded UI)
is functional.
