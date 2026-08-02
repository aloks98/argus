# Argus

> The hundred-eyed watchman.

**Centralized fleet management for homelabs** — "Cockpit, but centralized."
One control plane for observability, terminal access, and lifecycle
operations across all your VMs and LXCs, without SSH-ing into each guest.
Argus *sees and operates* the fleet; it is deliberately **not** a deployment
platform.

![Fleet overview](docs/screenshots/fleet.png)

## What it does

- **Live fleet dashboard** — every machine's status, CPU/memory sparklines,
  and failed-unit counts on one page, with search, tags, and grouping.
- **Metrics history** — CPU, memory, load, disk, and network charts per
  machine; plain Postgres storage, no time-series database to operate.
- **Docker & systemd** — container and unit state with start/stop/restart
  verbs, executed over the agent's authenticated stream and audited.
- **Logs on demand** — journald (whole system or per unit) and container
  logs, tailed live with priority/time filters and backward paging. Logs
  are pulled when you look, never shipped continuously.
- **Interactive terminal** — a real PTY in the browser (xterm.js),
  multiplexed over the same single connection as everything else.
- **Hardware inventory** — processor, cores, boot time/uptime,
  virtualization type (KVM vs LXC matters on a Proxmox fleet), disk,
  memory, swap.
- **Authentication** — any spec-compliant OIDC provider (only the issuer is
  provider-specific), plus a rate-limited local break-glass account for
  when SSO is down. Every verb writes an audit row.
- **Phone-friendly PWA** — check the fleet and restart a unit from your
  couch; installable to a home screen.

| | |
|---|---|
| ![Machine detail](docs/screenshots/machine.png) | ![System inventory](docs/screenshots/system.png) |

## How it works

```mermaid
flowchart LR
    subgraph guests [Every VM / LXC]
        A[argus-agent<br/>static musl binary]
    end
    subgraph cp [Control plane]
        S[argus<br/>Rust + embedded React UI]
        P[(Postgres)]
    end
    B[Browser] -->|HTTPS + OIDC| S
    A -->|one outbound mTLS gRPC stream| S
    S --- P
```

- The **agent** is a single static binary that dials *outbound* and holds
  one persistent mTLS gRPC stream. Metrics, container/unit state, logs, and
  terminal bytes are all multiplexed over it — one connection, one identity,
  no inbound ports on guests.
- Identity is an **internal CA**: enrollment exchanges a single-use join
  token (minted in the UI) for a client certificate; the CA's private key
  is encrypted at rest in Postgres. No external PKI.
- The **control plane** is stateless — all state lives in Postgres,
  migrations are embedded and run on startup. Kill it, restart it, move it;
  agents reconnect on their own.
- The browser surface and the agent gRPC surface are **separate
  endpoints** by design: humans arrive through your HTTPS proxy and OIDC;
  agents terminate mTLS directly in the process — client-cert verification
  is never delegated to a middlebox.

## Installing Argus

Tagged releases ship three ways. Tarballs and checksums attach to the
[Forgejo release page](https://git.nexus.e412.in/aloks98/argus/releases);
images and the Helm chart live on ghcr.io.

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
  ghcr.io/aloks98/argus:latest
```
The control plane is stateless and containerizes cleanly. The **agent**
doesn't — it needs host mounts (Docker socket, D-Bus, journal) that are easy
to forget and silently degrade a feature instead of failing loudly. Prefer
the tarball for agents; if you still want the container,
[`deploy/docker/README.md`](deploy/docker/README.md) has the full mount list
and the caveats.

### Helm (Kubernetes)
```bash
kubectl create secret generic argus-env \
  --from-literal=ARGUS_DATABASE_URL=postgres://argus:CHANGE_ME@postgres:5432/argus \
  --from-literal=ARGUS_FIELD_KEY="$(openssl rand -base64 32)" \
  --from-literal=ARGUS_PUBLIC_URL=https://argus.example.com

helm install argus oci://ghcr.io/aloks98/charts/argus --version 0.1.0
```
The chart never templates secret values — `existingSecret: argus-env` (the
name above) is a hard requirement, not a default to trust. See
`deploy/chart/argus/values.yaml` for the full set of values (ingress mode,
the mTLS gRPC `LoadBalancer` address, resources).

### Enrolling an agent
Whichever way you installed the control plane, agents join through the app:
open the fleet UI's **Enroll** page, mint a join token, and copy the exact
command it prints — the agent endpoint (interpolated from the server's own
`ARGUS_AGENT_SANS`, so it always matches the TLS certificate), the token, and
the CA certificate inlined — ready to paste onto the host verbatim. Don't
hand-type any of it; the page is the source of truth.

For scripted installs (cloud-init, config management, image baking), the CA
certificate is also served unauthenticated at `GET /ca.pem` on the browser
origin — it is a public certificate by definition, and a host being enrolled
has no session to present.

## Developing

```
crates/proto    gRPC contract (argus.proto) + codegen (protoc-free)
crates/common   shared constants/types
crates/server   control plane (bin: argus) + embedded migrations
crates/agent    guest agent (bin: argus-agent), musl-static
frontend/       Vite + React, embedded into argus via rust-embed
```

The frontend must be built before the server (it's embedded):
```bash
pnpm --dir frontend install --frozen-lockfile && pnpm --dir frontend run build
cargo build --release
```

- **Design of record:** [`docs/PRD.md`](docs/PRD.md)
- **Dev environment & operational notes:** [`docs/DEV.md`](docs/DEV.md)
- **Conventions:** [`CLAUDE.md`](CLAUDE.md)

This repo's canonical home is a self-hosted Forgejo; the GitHub repo is a
public mirror. Release artifacts (tarballs) attach to Forgejo Releases;
container images and the Helm chart live on ghcr.io either way.

## License

[AGPL-3.0](LICENSE). Run it, fork it, improve it — if you offer a modified
Argus to others over a network, share your changes.
