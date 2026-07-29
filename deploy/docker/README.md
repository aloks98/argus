# Argus container images

Two images: `Dockerfile.server` (the control plane, `argus`) and
`Dockerfile.agent` (the fleet agent, `argus-agent`). Both build from the
**repo root** as context (they need `crates/`, `frontend/`, and `Cargo.lock`
in scope):

```bash
docker build -f deploy/docker/Dockerfile.server -t argus:local .
docker build -f deploy/docker/Dockerfile.agent  -t argus-agent:local .
```

## The agent-container honesty section

**The static binary is the recommended install**, not this image. Argus is
"sees and operates the fleet without SSH-ing into each guest" -- but the
agent's own slices need direct access to host facilities that a container
only gets via *explicit bind mounts*, and a mount you forgot silently
degrades that slice instead of failing loudly:

- Docker state + container verbs (`bollard`) need the host's Docker socket.
- Systemd state + unit verbs (`zbus`) need the host's D-Bus system bus.
- Log tailing needs the host's journal and machine ID.

If you run the agent in a container anyway, mount all of these:

```bash
# Generate the field key ONCE and keep it: it encrypts the internal CA's
# private key at rest -- a fresh key on container recreation permanently
# orphans the CA and every enrolled agent.
FIELD_KEY="$(openssl rand -base64 32)"

docker run -d \
  --name argus-agent \
  --network host \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /run/dbus:/run/dbus \
  -v /var/log/journal:/var/log/journal:ro \
  -v /etc/machine-id:/etc/machine-id:ro \
  -e ARGUS_AGENT_ENDPOINT=https://agents.argus.lab.example:9443 \
  -e ARGUS_JOIN_TOKEN=<join-token-from-the-enroll-page> \
  argus-agent:local
```

`--network host` is required too: the agent dials outbound only, but several
of its facts (host network state) and the systemd/docker integrations assume
it's observing the host's own namespaces, not a container's.

For a real Flatcar/systemd host, prefer `deploy/packaging/install-agent.sh`
and the static `x86_64-unknown-linux-musl` binary directly -- see
`deploy/packaging/argus-agent.service` and `deploy/packaging/agent.env.example`.

## Running the server image

The server is stateless (state lives in Postgres) and doesn't need any host
mounts. Example:

```bash
docker run -d \
  --name argus \
  --network host \
  -e ARGUS_DATABASE_URL=postgres://argus:CHANGE_ME@localhost:5432/argus \
  -e ARGUS_FIELD_KEY="$FIELD_KEY" \
  -e ARGUS_PUBLIC_URL=https://argus.lab.example \
  -e ARGUS_HTTP_ADDR=0.0.0.0:8080 \
  -e ARGUS_AGENT_ADDR=0.0.0.0:9443 \
  argus:local
```

`ARGUS_DATABASE_URL`, `ARGUS_FIELD_KEY`, and `ARGUS_PUBLIC_URL` are required;
the server refuses to boot without them (see
`deploy/packaging/argus.env.example` for the full list, including optional
OIDC configuration, and `crates/server/src/config.rs` for the authoritative
source). Migrations are embedded and run automatically on startup -- no init
container needed.

The image exposes `8080` (browser HTTP surface, sits behind Traefik) and
`9443` (agent mTLS gRPC surface, sits behind a dedicated MetalLB
`LoadBalancer` -- never behind an HTTP proxy, per PRD §2.4). It also declares
a `HEALTHCHECK` against `/healthz` on `8080`.
