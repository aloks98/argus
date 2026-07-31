# Release engineering — design

Three distribution forms (bare metal, Docker, Kubernetes/Helm) plus the CI
that produces them. Context that shapes everything: **the repo will be
mirrored publicly to GitHub**, so artifacts target third-party consumers,
not only the homelab. CI remains Forgejo Actions (GitHub is a mirror, not a
build system); mirroring itself is Forgejo push-mirror repo settings — an
ops step documented in RELEASING.md, not a pipeline.

## Trigger & versioning

- Pushing a tag matching `v*` (e.g. `v0.2.0`) runs `release.yml`.
- The pipeline's first step verifies the tag equals the workspace
  `Cargo.toml` version (`v` + `workspace.package.version`); mismatch fails
  the release. No accidental drift between tag and binary-reported version.
- The existing `ci.yml` continues to gate PRs/main; `release.yml` is
  additive and tag-only.

## Artifact 1 — bare metal (Forgejo Release attachments)

- `argus-server-<tag>-x86_64-linux-gnu.tar.gz`: the `argus` binary
  (frontend embedded via the normal pnpm build → rust-embed pipeline),
  `argus.service` systemd unit, `argus.env.example` (every ARGUS_* var,
  commented), `install.sh` (copies binary to /usr/local/bin, unit to
  /etc/systemd/system, creates /etc/argus, chmod 600 the env file).
- `argus-agent-<tag>-x86_64-linux-musl.tar.gz`: the static agent binary
  (built exactly as CI's agent-musl job does, with the static-linking
  assertion), `argus-agent.service`, `agent.env.example`, `install.sh`
  matching the enroll page's documented layout (`/etc/argus/agent.env`,
  600).
- `SHA256SUMS` over both tarballs, attached beside them.
- The Forgejo Release is created by the pipeline with generated notes
  (commit range since the previous tag).

## Artifact 2 — Docker images (ghcr.io)

- `ghcr.io/<github-user>/argus:<tag>` + `:latest` — multi-stage:
  1. node stage: pnpm install + build the frontend (`dist/`);
  2. rust stage with cargo-chef (recipe → cook → build) for layer caching;
  3. runtime: `debian:bookworm-slim` + ca-certificates, non-root user,
     the binary only. HEALTHCHECK on `/healthz`.
- `ghcr.io/<github-user>/argus-agent:<tag>` + `:latest` — the musl binary
  in a minimal image (`scratch` or distroless-static). README documents the
  required host mounts (`/var/run/docker.sock`, D-Bus system bus socket,
  `/var/log/journal`, `/etc/machine-id`) and states plainly that the
  static-binary install is the recommended path — host-level probing is
  the agent's job, and a container obstructs it.
- **amd64 only at first release** — arm64 is a deliberate later addition
  (QEMU-cross Rust builds are slow; do it when someone needs it).
- Push auth: `GHCR_TOKEN` Forgejo Actions secret (GitHub PAT,
  `write:packages`) + `GHCR_USER`. Docker builds run on the `large` runner
  (the one with the docker label).

## Artifact 3 — Helm chart

`deploy/chart/argus/` (user decision: Helm, for public consumption):

- Deployment: single replica, `strategy: Recreate` (stateless, DB
  external — PRD §13), liveness `GET /healthz`, readiness `GET /readyz`
  (Postgres-gated), env from a Secret.
- Services: `argus-http` ClusterIP (browser), `argus-grpc` LoadBalancer
  with `values.grpc.loadBalancerIP` (MetalLB pin) — mTLS terminates in the
  pod, NEVER behind an HTTP proxy (PRD §2.4 must-not-reopen).
- Ingress, togglable: `ingress.mode: traefik` renders IngressRoute +
  cert-manager Certificate; `ingress.mode: ingress` renders a plain
  networking/v1 Ingress; `none` renders neither.
- Secrets: the chart REFERENCES an existing Secret
  (`existingSecret: argus-env`) carrying `ARGUS_DATABASE_URL`,
  `ARGUS_FIELD_KEY`, OIDC vars — it does not create secrets from values
  (no keys in Helm release history). values.yaml documents the field-key
  generation one-liner.
- Postgres is external by design; the chart takes a DSN (CloudNativePG,
  RDS, whatever). No bundled database.
- Published two ways per release: OCI push
  (`oci://ghcr.io/<github-user>/charts/argus`, chart version = app
  version = tag) and the packaged `.tgz` attached to the Forgejo Release.
- `helm lint` + `helm template` sanity render in CI.

## release.yml shape

Tag push → three jobs:
1. `artifacts` (medium): tag/Cargo.toml version check → build both
   tarballs (+ static-linking assertion for the agent) → SHA256SUMS →
   create the Forgejo Release via API with all attachments.
2. `images` (large): buildx both images, push `:tag` + `:latest` to ghcr.
3. `chart` (small): helm lint/template/package → OCI push → attach .tgz
   to the Release (after job 1 creates it — `needs: artifacts`).

Operator inputs needed once: `GHCR_USER` + `GHCR_TOKEN` secrets; GitHub
namespace choice; push-mirror configuration.

## Also in scope

- `RELEASING.md`: the runbook — bump workspace version, tag, push tag,
  what appears where; secret setup; mirror setup; first-release checklist.
- README gains an install section per distribution form (binary tarball,
  docker run, helm install) — brief, pointing at RELEASING.md and the
  enroll-page flow for agents.

## Out of scope

- arm64 images (later, on demand).
- GitHub Actions workflows (GitHub is a mirror).
- .deb/.rpm packaging (tarballs chosen).
- Auto-update channels for the agent (the self-update slice owns that;
  its artifact source will be these releases).
- Signing (cosign/minisign) — worth revisiting once public adoption is
  real; noted in RELEASING.md as a follow-up.

## Testing

- The pipeline can't be fully tested without a tag: a `workflow_dispatch`
  trigger with a `dry_run` input builds everything but publishes nothing —
  the release rehearsal. First real release = `v0.1.0`.
- Helm: `helm lint` + `helm template` with default and traefik-mode values
  in the `checks` CI job (cheap, every PR — the chart can't rot).
- Dockerfiles built (no push) in PR CI only when they change
  (paths filter), on the large runner.
- Local verification for this slice: build both tarball layouts and both
  images locally, run the server image against the dev Postgres, install
  the agent tarball on the dev guest via its install.sh.
