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
| Docker images | `ghcr.io/aloks98/argus`, `ghcr.io/aloks98/argus-agent` | GitHub Container Registry, independent of either git host |
| Helm chart (OCI) | `oci://ghcr.io/aloks98/charts/argus` | same GHCR namespace |

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
   (`https://github.com/aloks98/argus.git`) and a GitHub PAT with
   `repo` scope for authorization. This is what makes the tag itself (and
   the rest of the source) show up on GitHub — it does not touch Releases or
   packages, see the table above.
3. **Nothing — the auto-issued token works.** `release.yml` uses the
   workflow's auto-issued `secrets.GITHUB_TOKEN` (Forgejo Actions'
   GitHub-Actions-compatible token) to create the Release and upload
   assets, and the v0.1.0 tag push **empirically confirmed** it carries
   release-creation scope on this instance. (Note Forgejo ignores the
   GitHub-style `permissions:` field entirely — token capability comes
   from the instance/repo "Authorized Integrations" settings.) If a future
   Forgejo upgrade or settings change makes it 403, add a Forgejo personal
   access token (Settings → Applications → Generate New Token,
   `write:repository` scope) as a `RELEASE_TOKEN` secret and swap the
   three `env: TOKEN: ${{ secrets.GITHUB_TOKEN }}` lines in `release.yml`
   ("Adopt or create Forgejo release" and "Upload release assets" in the
   `artifacts` job, "Attach chart to release" in the `chart` job) to
   `${{ secrets.RELEASE_TOKEN }}`.
4. **ghcr package visibility.** GHCR creates packages **private** on first
   push. After the first release (or after adding a new package), flip
   `argus`, `argus-agent`, and `charts/argus` to public on github.com →
   profile → Packages → package settings → Change visibility. Until then,
   anonymous `docker pull` / `helm install` fail with 403/denied.

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
4. **Watch the four jobs** (`version` → `artifacts` + `images` in parallel →
   `chart`, which needs both `version` and `artifacts`): Forgejo Actions UI
   at `git.nexus.e412.in/aloks98/argus/actions`, or `fj actions tasks`.
5. **What lands where** once green: bare-metal tarballs (`argus-server-*`,
   `argus-agent-*`) + `SHA256SUMS` attached to the Forgejo Release for the
   tag; `argus`/`argus-agent` pushed to `ghcr.io/aloks98/...` as both
   `:vX.Y.Z` and `:latest`; the packaged chart `.tgz` pushed to
   `oci://ghcr.io/aloks98/charts/argus` **and** attached to the same
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

## Per-release checklist

(v0.1.0 shipped 2026-07-31, run #105, all four jobs green on the first tag
push — Release created by the auto token, images and chart on ghcr.)

- [ ] Version bumped in workspace `Cargo.toml`, merged to `main`.
- [ ] Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z` — or use
      Forgejo's **New release** page (see below).
- [ ] Watch all four jobs go green (`fj actions tasks`).
- [ ] Verify the Forgejo Release has the tarballs, `SHA256SUMS`, and the
      chart `.tgz` attached. If a create/upload step 403s (it didn't for
      v0.1.0), switch to `RELEASE_TOKEN` (prerequisite 3), then delete and
      re-push the tag — a push event can't be re-dispatched:
      `git push origin :vX.Y.Z && git tag -d vX.Y.Z`, fix the secret, then
      re-tag and push.
- [ ] Verify `ghcr.io/aloks98/argus:vX.Y.Z` and `:latest` pull
      **anonymously** (new packages start private — prerequisite 4).
- [ ] Same for `ghcr.io/aloks98/argus-agent` and the chart:
      `helm install argus oci://ghcr.io/aloks98/charts/argus --version X.Y.Z`.
- [ ] Spot-check a tarball's `install.sh` on a real host after notable
      packaging changes.

Equally valid: Forgejo's **New release** page (tag `vX.Y.Z` @ `main`,
Publish). The page creates the tag, the tag fires the pipeline, and the
pipeline **adopts** your Release — your description is preserved and the
assets are attached to it. Don't upload files on that page; the pipeline
attaches the real artifacts.


### The GITHUB_TOKEN question (answered)

Resolved by the real v0.1.0 tag push: the auto-issued `secrets.GITHUB_TOKEN`
**can** create Releases and attach assets on this instance. Two findings
worth keeping:

- Forgejo **ignores** GitHub's `permissions:` field (it warns "not
  supported… will be ignored" per job), so `release.yml` doesn't carry one.
  Token capability is governed by Forgejo's "Authorized Integrations"
  settings, not the workflow file.
- Dry runs never reach the push-gated publish steps, so token scope can
  only be tested by a real tag. If it ever 403s, the `RELEASE_TOKEN`
  fallback is documented in prerequisite 3 above.

## Follow-ups (deliberately out of scope for v0.1.0)

- **arm64 images** — amd64-only for now; QEMU-cross Rust builds are slow
  enough to defer until someone actually needs an arm64 host.
- **Signing (cosign/minisign)** — worth revisiting once public adoption is
  real; noted here as the follow-up the design doc points at.
- **.deb/.rpm packaging** — tarballs were chosen deliberately; revisit only
  if distro packaging becomes a real ask.
- **Auto-update channels for the agent** — belongs to a future self-update
  slice, not this pipeline.
