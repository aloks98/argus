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

## Installing Argus

Tagged releases ship three ways — see [`RELEASING.md`](RELEASING.md) for how
they're built and exactly where each one lives.

### Binary tarball
```bash
curl -LO https://git.nexus.e412.in/aloks98/argus/releases/download/v0.1.0/argus-server-v0.1.0-x86_64-linux-gnu.tar.gz
tar xzf argus-server-v0.1.0-x86_64-linux-gnu.tar.gz
cd argus-server-0.1.0 && sudo ./install.sh
```
Installs the `argus` binary, a systemd unit, and `/etc/argus/argus.env`
(seeded from the example, never overwritten on re-install). Edit the env
file, then `sudo systemctl enable --now argus`.

### Docker
```bash
# Generate the field key ONCE and keep it: it encrypts the internal CA's
# private key at rest -- a fresh key on container recreation permanently
# orphans the CA and every enrolled agent.
FIELD_KEY="$(openssl rand -base64 32)"

docker run -d --name argus --network host \
  -e ARGUS_DATABASE_URL=postgres://argus:CHANGE_ME@localhost:5432/argus \
  -e ARGUS_FIELD_KEY="$FIELD_KEY" \
  -e ARGUS_PUBLIC_URL=https://argus.lab.example \
  ghcr.io/<github-user>/argus:latest
```
The control plane is stateless and containerizes cleanly. The **agent**
doesn't — it needs host mounts (Docker socket, D-Bus, journal) that are easy
to forget and silently degrade a slice instead of failing loudly. Prefer the
tarball above for agents; if you still want the container,
[`deploy/docker/README.md`](deploy/docker/README.md) has the full mount list
and the caveats.

### Helm (Kubernetes)
```bash
kubectl create secret generic argus-env \
  --from-literal=ARGUS_DATABASE_URL=postgres://argus:CHANGE_ME@postgres:5432/argus \
  --from-literal=ARGUS_FIELD_KEY="$(openssl rand -base64 32)" \
  --from-literal=ARGUS_PUBLIC_URL=https://argus.example.com

helm install argus oci://ghcr.io/<github-user>/charts/argus --version 0.1.0 \
  --set image.repository=ghcr.io/<github-user>/argus
```
The chart never templates secret values — `existingSecret: argus-env` (the
name above) is a hard requirement, not a default to trust. See
`deploy/chart/argus/values.yaml` for the full set of values (ingress mode,
the mTLS gRPC `LoadBalancer` address, resources).

### Enrolling an agent
Whichever way you installed the control plane, agents join through the app:
open the fleet UI's **Enroll** page, mint a join token, and copy the exact
command it prints — endpoint, token, and CA certificate download, ready to
paste onto the host. Don't hand-type `ARGUS_JOIN_TOKEN`; the page is the
source of truth for it.

> **Status:** actively developed. Core slices are live — enrollment/mTLS,
> metrics, Docker + systemd state and verbs, log tailing, interactive
> terminal, OIDC + local-admin auth, machine inventory, and a responsive
> PWA frontend. Design of record: [`docs/PRD.md`](docs/PRD.md).
