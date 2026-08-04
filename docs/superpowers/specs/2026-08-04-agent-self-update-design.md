# Agent self-update — design

Roadmap item 3. Push the control plane's bundled agent binary to an enrolled
machine over the existing mTLS `Session`, verify, swap, re-exec. Supersedes the
PRD §"Self-update" sketch (URL + `GET /agent/binary/:arch/:version`): since
that sketch, the stream has grown flow control and proven bulk throughput
(PTY firehose, log batching), so the binary travels as chunks on the one
already-authenticated channel instead of a second protocol on the mTLS
listener. `UpdateAgent.url` is documented as superseded and never set.

## Confirmed decisions

- **Binary source: bundled with the server.** The release pipeline already
  builds both binaries from one workspace version; the server image carries
  its matching musl agent. No forge fetch, no upload UI.
- **Trigger: per-machine button + outdated badge.** No update-all.
- **Rollback: `.old` beside the binary, manual.** No watchdog/auto-revert.
- **Transport: chunks over the existing Session stream** (approach A above).

## Proto (additive only)

- `UpdateAgent` gains `uint64 total_bytes = 4`, `string command_id = 5`, and
  `string issued_by = 6` (the last two mirroring the `Command` envelope —
  `UpdateAgent` is a bare `ServerFrame` arm, so unlike verbs it must carry
  its own correlation id and actor for the agent-side trail). `url = 1` is
  kept for wire compat, never set, and commented as superseded by streamed
  chunks.
- New `ServerFrame` arm: `UpdateChunk { bytes data = 1; bool last = 2; }`.
  256 KiB chunks (~30 frames for the ~8 MB musl binary). At most one update
  in flight per session, so chunks need no sequence numbers or correlation —
  ordering is the stream's ordering.
- The trigger rides the verb machinery: `command_id` correlation, agent
  answers with the existing `CommandResult` ("staged <version>" or a refusal).

## Server

**Bundling.** Optional env `ARGUS_AGENT_BINARY` (path to the musl agent).
`deploy/docker/Dockerfile.server` COPYs the release agent in and pre-sets the
env. Dev points it at a locally built binary. On first use the server reads
the file and caches `{bytes, sha256, size}`; sha256 via `ring` (in tree). The
bundled binary's version IS the server's version by construction
(`argus_common` / workspace version) — no version probing of the file.

**Endpoint.** `POST /api/machines/:id/agent-update` (authed `/api` router):

| Guard | Response |
|---|---|
| no `ARGUS_AGENT_BINARY` configured / unreadable | 503 "no agent binary bundled" |
| agent not connected | 409 (same `DispatchError::NotConnected` path as verbs) |
| machine `arch` ≠ `x86_64` | 409 "unsupported arch" (we bundle one arch) |

Happy path: audit `agent.update` (target_ref = version, `command_id`
correlation, result updated by `CommandResult` like every verb), send
`UpdateAgent { version, sha256, total_bytes }`, then stream chunks through the
session's existing bounded outbound channel — its capacity is the
backpressure; no new flow-control code. Bounded wait ~60 s for the
`CommandResult`; 200 `{ "staged": true }` on ok, 502-style error body on a
refusal, 504 on timeout (unconfirmed — the agent may still stage). Proof of
success is the agent reconnecting and reporting the new `agent_version` in
`Hello`. Same-version pushes are allowed (repair path); the UI just doesn't
prompt for them.

**Server info for the UI.** New tiny `GET /api/server-info` →
`{ version, agent_update: { version, sha256 } | null }` (null = nothing
bundled). Cached client-side like `enrollment-config` (staleTime ∞ — fixed at
boot).

## Agent

New module `crates/agent/src/update.rs`; the session loop routes
`UpdateAgent`/`UpdateChunk` frames to it.

Staging sequence:
1. Resolve own path via `/proc/self/exe` **before** any renaming.
2. Refuse (CommandResult ok=false) if: an update is already in flight, or
   `total_bytes` is 0 or > 64 MB (implausibility cap).
3. Stream chunks to `<binary_dir>/.argus-agent.update` (same filesystem —
   atomic rename requirement). Track received size against `total_bytes`;
   over- or under-run at `last` is a refusal.
4. On `last`: fsync → sha256 (`ring`, no new deps) → mismatch: delete temp,
   refuse. Match: `chmod 755`; rename current binary → `argus-agent.old`
   (overwriting any previous `.old`); rename temp → binary path.
5. `CommandResult { ok: true, "staged <version>" }`, brief flush delay, then
   `exec()` itself: `std::os::unix::process::CommandExt::exec` on the (new)
   binary path with the ORIGINAL argv and env — pid preserved, so the systemd
   unit never notices, and `--config` survives verbatim.

Every failure path leaves the running binary untouched and the session alive.
Downgrades are just updates — which is also what makes `.old` + re-push a
recovery story. Manual rollback one-liner (documented in DEV.md):
`sudo mv /path/argus-agent.old /path/argus-agent && sudo systemctl restart argus-agent`
(or re-exec by hand in dev).

## UI

- Machine page: when online AND `arch == "x86_64"` AND
  `agent_version ≠ server-info.agent_update.version`, show a warn-tone
  `agent v<X.Y.Z> available` badge plus an "Update agent" button. The button
  opens the protected-verb-style confirm dialog: "Replaces the agent binary
  and re-execs it — this machine drops and re-establishes its connection."
  Spinner while staging; after 200 the existing `reconnecting…` machinery
  covers the gap until the new `Hello` lands and the badge clears on poll.
- Fleet page: outdated rows get a quiet warn-tone `agent outdated` text beside
  status (StatusBadge, text only — no fill; exceptions stay the loud ones).
- Nothing bundled (`agent_update: null`): no badge, no button, anywhere.

## Testing

- **Agent unit tests** around a `stage()` seam (everything except the final
  `exec`): chunk assembly; hash verify; mismatch refusal leaves the original
  binary untouched and deletes the temp; `.old` created on success; size
  over/under-run refusals. All in a tempdir.
- **Server**: oneshot tests for 503 (unbundled) / 409 (offline) / 409 (arch) /
  200 matrix; fake-session seam test asserting the frame sequence —
  `UpdateAgent` first, chunk bytes concatenate to exactly `total_bytes`, and
  their sha256 equals the announced hash.
- **Frontend**: typecheck + lint + fmt + build.
- **Live E2E on fatman** (recorded in DEV.md): point `ARGUS_AGENT_BINARY` at a
  patch-bumped build → Update → agent re-execs (same pid), reconnects,
  reports the new version, `.old` on disk; then the rollback one-liner
  restores the previous version. Deliberate failure: announce a corrupted
  sha256 via seam test → refusal, session stays up, audit row `result=error`.

## Non-goals

- Update-all / fleet-wide rollout, scheduling, or staged canaries.
- Non-x86_64 binaries (single-arch homelab; the arch guard makes this safe).
- Auto-rollback watchdog.
- Agent-initiated update checks (server pushes; the agent never polls).
