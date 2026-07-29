# Argus

> The hundred-eyed watchman.

Centralized fleet management for a multi-Proxmox-host homelab — "Cockpit, but
centralized". One control plane for observability, terminal access, and lifecycle
operations across many VMs and LXCs, without SSH-ing into each guest. Argus *sees
and operates* the fleet; it is **not** a deployment platform.

- **Design of record:** [`docs/PRD.md`](docs/PRD.md)
- **Conventions & the decisions that must not be reopened:** [`CLAUDE.md`](CLAUDE.md)

## Architecture at a glance
- Single Rust **control plane** (`argus`) with an embedded React frontend; stateless,
  all state in Postgres.
- Thin Rust **agent** (`argus-agent`) on each guest that dials outbound and holds one
  persistent **mTLS gRPC** stream; everything (metrics, docker/systemd state, logs,
  terminal) is multiplexed over it.
- Internal CA (no external dependency); Postgres via `sqlx` with embedded migrations.

## Layout
```
crates/proto    gRPC contract (argus.proto) + codegen
crates/common   shared constants/types
crates/server   control plane (bin: argus) + embedded migrations
crates/agent    guest agent (bin: argus-agent)
frontend/       Vite + React + @e412/rnui-react, embedded into argus
```

## Build
The frontend must be built before the server (it is embedded via `rust-embed`):
```bash
pnpm --dir frontend install --frozen-lockfile && pnpm --dir frontend run build
cargo build --release
```

> **Status:** PRD + skeleton. Subsystems are stubbed with their intended shape;
> implementation starts with the "Spine" slice (enroll → mTLS → heartbeat → fleet
> page). See `CLAUDE.md`.
