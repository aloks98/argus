# Releasing Argus

Tag-driven pipeline (`.forgejo/workflows/release.yml`) that produces three
distribution forms from one `vX.Y.Z` tag: bare-metal tarballs, Docker images,
and a Helm chart. This document is the runbook; the workflow file's own
header comment carries the deeper "why" for anything that looks unusual —
read both if something here is ambiguous.

**Where things live, since the git host and the artifact hosts differ:**
this repo is hosted on a self-hosted Forgejo (`git.nexus.e412.in`) and
**mirrored publicly to GitHub** via a Forgejo push-mirror. That mirror only
replicates git objects (commits, branches, tags) — it does **not** replicate
Forgejo Releases. So:

| Artifact | Lives at | Reachable from |
|---|---|---|
| Source + tags | Forgejo (origin) | mirrored to GitHub too |
| Release (tarballs, SHA256SUMS, chart `.tgz`) | Forgejo Release page (`.../releases`) | Forgejo only — the GitHub mirror shows the tag but no attached Release assets |
| Docker images | `ghcr.io/<github-user>/argus`, `ghcr.io/<github-user>/argus-agent` | GitHub Container Registry, independent of either git host |
| Helm chart (OCI) | `oci://ghcr.io/<github-user>/charts/argus` | same GHCR namespace |

Point people at the Forgejo release page for tarballs, not GitHub's tag view.

## Prerequisites (one-time, before the first release)

1. **`GHCR_USER` / `GHCR_TOKEN` Forgejo Actions secrets.** Repo → Settings →
   Actions → Secrets → add both. `GHCR_USER` is your GitHub username;
   `GHCR_TOKEN` is a GitHub Personal Access Token with `write:packages`
   scope (classic PAT, or a fine-grained PAT with "Packages: read and
   write"). Required by the `images` and `chart` jobs — both fail fast with
   a clear message if either is unset when a real release runs.
2. **GitHub push-mirror.** Repo → Settings → Repository → "Mirror Settings" →
   "Add Push Mirror": the GitHub remote URL
   (`https://github.com/<github-user>/argus.git`) and a GitHub PAT with
   `repo` scope for authorization. This is what makes the tag itself (and
   the rest of the source) show up on GitHub — it does not touch Releases or
   packages, see the table above.
3. **Optional `RELEASE_TOKEN` secret.** `release.yml` uses the workflow's
   auto-issued `secrets.GITHUB_TOKEN` (Forgejo Actions' GitHub-Actions-
   compatible token) to create the Release and upload assets, requested via
   `permissions: contents: write`. Whether this instance's Forgejo Actions
   token actually carries release-creation scope is **not yet empirically
   verified** — see "The GITHUB_TOKEN question" below. If it 403s, add a
   Forgejo personal access token (Settings → Applications → Generate New
   Token, `write:repository` scope) as a `RELEASE_TOKEN` secret and swap the
   three `env: TOKEN: ${{ secrets.GITHUB_TOKEN }}` lines in `release.yml`
   ("Create Forgejo release" and "Upload release assets" in the `artifacts`
   job, "Attach chart to release" in the `chart` job) to
   `${{ secrets.RELEASE_TOKEN }}`.
4. **`deploy/chart/argus/values.yaml`'s `image.repository`** ships as the
   placeholder `ghcr.io/CHANGEME/argus`. Update it to the real
   `ghcr.io/<github-user>/argus` as part of the first release (see the
   checklist below) — until then, `helm install`/`helm template` need an
   explicit `--set image.repository=...` override.

## The release runbook

1. **Bump the version.** Workspace `Cargo.toml`'s `version` field (currently
   `0.1.0`) is the single source of truth — the chart's `Chart.yaml`
   `version`/`appVersion` and the tarball/image tags are all stamped from
   the tag at release time, not from anything committed. Open a PR that
   bumps it.
2. **Merge the PR to `main`.**
3. **Tag the merge commit** — must be `v` + the exact `Cargo.toml` version,
   or the pipeline's `version` job hard-fails before anything builds:
   ```bash
   git checkout main && git pull
   git tag v0.1.0
   git push origin v0.1.0
   ```
4. **Watch the three jobs** (`version` → `artifacts` + `images` in parallel →
   `chart`, which needs both `version` and `artifacts`): Forgejo Actions UI
   at `git.nexus.e412.in/aloks98/argus/actions`, or `fj actions tasks`.
5. **What lands where** once green: bare-metal tarballs (`argus-server-*`,
   `argus-agent-*`) + `SHA256SUMS` attached to the Forgejo Release for the
   tag; `argus`/`argus-agent` pushed to `ghcr.io/<github-user>/...` as both
   `:vX.Y.Z` and `:latest`; the packaged chart `.tgz` pushed to
   `oci://ghcr.io/<github-user>/charts/argus` **and** attached to the same
   Forgejo Release.

## CI environment notes

Two runner-specific quirks shape `release.yml`'s structure — both are
commented inline where they matter, collected here for a quick read before
touching the file:

- **Container jobs get `sh` (dash), not `bash`, unless pinned.** act_runner
  runs a job-level `container:`'s steps with `sh` by default, so any
  `set -euo pipefail` (a bashism) dies instantly with "Illegal option -o
  pipefail". `release.yml` (and `ci.yml`) set `defaults: run: shell: bash`
  workflow-wide to fix this — don't drop it, and don't assume a new step
  inherits bash from context alone.
- **`$GITHUB_OUTPUT` writes fail inside container jobs on this runner.** A
  step with `id:` that writes `$GITHUB_OUTPUT` reproducibly fails when its
  job has a `container:`, confirmed by bisection (same step content
  succeeds immediately with `container:` removed). `release.yml` works
  around this by computing the version in its own uncontainerized `version`
  job (consumed by the others via `needs.version.outputs.*`, which is
  unaffected — that substitution happens before a container step starts),
  and by using plain files in the shared job workspace for `artifacts`'s own
  step-to-step handoffs (release notes JSON, the created release's id)
  instead of `$GITHUB_OUTPUT`. `chart` avoids needing a `release_id` output
  from `artifacts` at all by looking the release up by tag via the Forgejo
  API instead.
- **The `large` runner has no docker daemon inside job containers.** `images`
  runs directly on the runner host (no `container:` key) rather than inside
  `catthehacker/ubuntu:...` — with a `container:`, `docker build` fails in
  ~1 minute with "failed to connect to the docker API", confirmed by
  reproducing the same image locally without bind-mounting
  `/var/run/docker.sock`. `ci.yml`'s `docker-build` job likely has the same
  latent issue; not yet fixed there (out of scope for this pipeline).

## The dry-run rehearsal

`workflow_dispatch` runs every job's build steps but skips every publish step
(Release creation, asset upload, docker push, helm push) — gated by
`if: github.event_name == 'push'` throughout, not by the dispatch form's
`dry_run` input (which exists only to document intent; the real gate is the
event type). Confirmed to work from a non-`main` branch on this Forgejo
instance, so you don't need to merge first to rehearse:

```bash
fj actions dispatch release.yml <branch>
# or, to be explicit about intent (not actually read by the workflow):
fj actions dispatch release.yml <branch> -I dry_run=true
```

**Status as of this writing** (most recent full rehearsal, run #94, on
`release-slice`):
- `version` — green, ~30s.
- `artifacts` — green, ~4m42s, builds the real tarballs (musl toolchain +
  `package-release.sh` unmodified; release-creation steps correctly skipped).
- `images` — green, ~6m, both Dockerfiles build; GHCR login/push correctly
  skipped.
- `chart` — green, ~1m10s (`helm lint`/`helm package`; OCI push and asset
  upload correctly skipped on dispatch).

Re-run the rehearsal after any workflow edit — this pipeline has already
hidden two runner-specific bugs that only a real dispatch surfaced (see "CI
environment notes" above).

## First-release checklist (v0.1.0)

- [ ] `GHCR_USER` / `GHCR_TOKEN` secrets set (prerequisite 1).
- [ ] GitHub push-mirror configured (prerequisite 2).
- [ ] `deploy/chart/argus/values.yaml`'s `image.repository` updated from
      `ghcr.io/CHANGEME/argus` to the real namespace (prerequisite 4) —
      either before tagging, or accept that `helm install` needs
      `--set image.repository=...` until a follow-up release fixes it.
- [ ] Confirm workspace `Cargo.toml` `version` is `0.1.0` (it already is).
- [ ] Tag and push: `git tag v0.1.0 && git push origin v0.1.0`.
- [ ] Watch all three jobs go green.
- [ ] **Verify the Forgejo Release was actually created and assets
      uploaded** — this is the first real test of the GITHUB_TOKEN question
      below. If "Create Forgejo release" or "Upload release assets" 403s,
      switch to `RELEASE_TOKEN` (prerequisite 3), then delete and re-push the
      tag to try again — a push event can't be re-dispatched like a
      `workflow_dispatch` run:
      `git push origin :v0.1.0 && git tag -d v0.1.0`, fix the secret, then
      `git tag v0.1.0 && git push origin v0.1.0`.
- [ ] Verify `ghcr.io/<github-user>/argus:v0.1.0` and `:latest` pull.
- [ ] Verify `ghcr.io/<github-user>/argus-agent:v0.1.0` and `:latest` pull.
- [ ] Verify `helm install argus oci://ghcr.io/<github-user>/charts/argus
      --version 0.1.0` works against a real cluster.
- [ ] Download a tarball from the Forgejo Release page and run its
      `install.sh` on a real host.

### The GITHUB_TOKEN question

`release.yml` requests `permissions: contents: write` so the workflow's
auto-issued `secrets.GITHUB_TOKEN` can create the Release and attach assets
— that's GitHub's permission model, which Forgejo Actions mirrors, but
**whether this specific Forgejo instance's Actions token actually carries
release-creation scope has not been empirically proven**: no dry run reaches
the push-gated release steps (dispatch runs never publish), so the first
real `v0.1.0` tag push is the first real test. If it 403s, this is expected
and already has a documented fallback — see prerequisite 3 above.

## Follow-ups (deliberately out of scope for v0.1.0)

- **arm64 images** — amd64-only for now; QEMU-cross Rust builds are slow
  enough to defer until someone actually needs an arm64 host.
- **Signing (cosign/minisign)** — worth revisiting once public adoption is
  real; noted here as the follow-up the design doc points at.
- **.deb/.rpm packaging** — tarballs were chosen deliberately; revisit only
  if distro packaging becomes a real ask.
- **Auto-update channels for the agent** — belongs to a future self-update
  slice, not this pipeline.
