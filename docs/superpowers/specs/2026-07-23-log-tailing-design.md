# Log tailing slice — design

**Date:** 2026-07-23
**Build order:** slice 5 of 6 (PRD §8) — "Log tailing (journal + docker, pull-on-demand)".
**Depends on:** Spine (#1), Metrics (#2), Docker (#3), frontend design system (#4),
Systemd (#5) — all merged.

## Goal

End-to-end, independently testable: an operator opens a unit's journal or a
container's logs from the machine page, sees a live tail streamed from the
agent, and closing the view stops the tail on the agent. Access is audit-logged.

## Scope

- **In:** journal tailing per systemd unit, Docker log tailing per container, an
  SSE browser surface, and a log viewer usable as a drawer or a full page.
- **Out:** the terminal (slice 6). No log *storage*, search, or retention —
  continuous log shipping is a PRD "forever" non-goal (§1); logs are read live
  from the source or not at all. No cross-machine log aggregation.

## Proto

**No proto change.** `LogTailRequest { request_id, source, tail_lines, follow }`,
`LogTailStop { request_id }`, and `LogChunk { request_id, data, eof }` already
exist, as do the `ServerFrame.log_tail_start` / `log_tail_stop` and
`AgentFrame.log_chunk` payloads. Like the two slices before it, this fills in
behavior around a contract that is already the source of truth.

`LogChunk.data` carries **NDJSON** — one JSON object per line:

```json
{"ts":1784812931123,"level":3,"ident":"nginx","msg":"connect() failed"}
```

- `ts` — milliseconds since epoch.
- `level` — syslog priority `0..=7`, or `null` when the source has no concept of
  severity (Docker). The viewer colours by this and must handle `null`.
- `ident` — syslog identifier / container name; `null` when unknown.
- `msg` — the line itself.

A framing marker uses the same envelope with `level: 4` (warning) and a
`marker: true` field, so the drop notice below flows through the ordinary
rendering path rather than needing a second channel.

## Decisions

- **Both sources in one slice.** The transport, SSE endpoint, disconnect
  lifecycle and viewer are shared and constitute most of the work; `bollard` is
  already a dependency and exposes a log stream, so Docker is a small addition
  rather than a second build-out.
- **NDJSON rather than raw bytes**, so the viewer can colour by severity and
  show a consistent timestamp. `journalctl -o json` supplies `PRIORITY`,
  `__REALTIME_TIMESTAMP`, `SYSLOG_IDENTIFIER` and `MESSAGE`. Docker lines have no
  severity and render plainly through the same path — the format has to tolerate
  both regardless, so it is defined once.
- **The journal is read via a `journalctl` subprocess, not `libsystemd`.**
  Linking `libsystemd` would break the static-musl requirement (CLAUDE.md), and
  systemd exposes no stable D-Bus API for reading journal *content* — the `zbus`
  connection this slice inherits cannot serve it. A subprocess costs a fork per
  tail and is bounded by the tail's lifetime.
- **Logs are best-effort; the Session is not.** Chunks share the one Session
  stream with heartbeats, metrics and verb results, on a 16-slot channel. The
  agent batches lines into one frame per ~100ms, and if the channel is still
  full it **drops the batch via `try_send`**, counts it, and injects a
  `— N lines dropped (stream saturated) —` marker. A flooding unit must never
  delay a heartbeat, because the 45s offline sweeper would then mark a healthy
  machine offline. The operator always sees that a gap occurred rather than
  silently reading an incomplete log.
- **This stays off the 15s heartbeat tick.** The tick already serializes two
  5s-capped collectors (docker, systemd), leaving roughly a 1.8× margin against
  the sweeper; the systemd slice's review flagged that a third collector would
  erode it further. Tailing is event-driven in its own task and adds nothing to
  that path.
- **The tail's lifetime is the browser connection.** Closing the SSE stream
  sends `LogTailStop`, and the agent kills the subprocess / drops the Docker
  stream. Session teardown cancels every tail for that machine.
- **Opening a tail is audited** as `logs.open` with the source as `target_ref`.
  It is a read rather than a mutation, but the PRD already treats
  `terminal.open` as auditable, and logs can expose secrets — who read what is
  worth recording. There is no terminal result to update, so the row is written
  once as `ok`.
- **No dedupe across reconnects.** `EventSource` reconnects on its own (the
  viewer library manages this), and a reconnect opens a *fresh* tail on the
  server that re-sends `tail_lines`, so a few lines may repeat. Suppressing that
  would require sequence tracking the pull-on-demand model does not carry. Note
  the reconnect also leaves the *previous* tail to be cleaned up by the
  disconnect guard — the two are independent, and the guard is what prevents an
  orphaned `journalctl -f`.
- **`follow=false` is a snapshot.** The agent emits the last `tail_lines` and
  then a chunk with `eof: true`; the server closes the SSE stream and the viewer
  shows a static log with no "following" indicator. `follow=true` (the default
  from the UI) never sends `eof` until the tail is stopped.
- **Tails are per-request, never shared.** Two browser tabs on the same unit open
  two `request_id`s and two subprocesses. Deduplicating them would couple
  unrelated viewers' lifetimes — closing one tab must not truncate the other —
  and a handful of concurrent tails is well within budget for a homelab fleet.

## Security

This is the first slice where a browser-supplied string reaches a **process
argument**, so it gets explicit treatment rather than being assumed safe.

- `source` is parsed into a typed enum (`Journal(unit)` / `Docker(id)`) and
  validated **twice** — server-side before dispatch, and agent-side before use.
  Neither trusts the other.
- Unit names are validated against systemd's charset — ASCII alphanumerics plus
  `: _ . @ - \` — with a length cap; container references against alphanumerics
  plus `_ . -` (Docker's own name and id charset). The backslash is deliberate:
  systemd escapes device-backed unit names (`dev-disk-by\x2duuid-…`). Both
  validators (server and agent) allow exactly the same set, so the
  double-validation trust boundary holds. Anything else is a `400`.
- The subprocess is spawned with **argv directly** (`tokio::process::Command`
  with `.arg()`), never through a shell. There is no string interpolation into a
  command line anywhere in this slice.
- `tail_lines` is clamped server-side to a sane maximum (1000) so a request
  cannot ask the agent to render an unbounded backlog.

## Agent

### New module `logs.rs`

- `LogTailer` — owns one tail. Spawned per `request_id`, holding either a
  `tokio::process::Child` (journal) or a bollard log stream (docker), plus the
  batching timer.
- `Registry` — `HashMap<String, AbortHandle>` keyed by `request_id`, so
  `LogTailStop` cancels precisely one tail and session teardown cancels all of
  them. Without it a dropped session leaves `journalctl -f` running forever.
- `parse_source(&str) -> Result<Source, SourceError>` — pure; the agent-side
  half of the validation above.
- `journal_line(&str) -> Option<LogLine>` — pure; maps one `journalctl -o json`
  record to the NDJSON envelope, tolerating missing fields and a `MESSAGE` that
  systemd renders as a byte array rather than a string.
- `Batcher` — accumulates lines, flushes on ~100ms or a size threshold, and
  reports how many lines it dropped so the marker can be emitted. Pure enough to
  test without a running tail.

### `session.rs`

- Inbound gains `LogTailStart` (spawn a tailer, register it) and `LogTailStop`
  (cancel by `request_id`).
- The registry is cancelled wholesale when `connect_and_serve` returns, beside
  the existing `sender.abort()`.

## Server

### `hub.rs`

- `tails: Mutex<HashMap<String, mpsc::Sender<LogChunk>>>` — `request_id` → the
  SSE sink. A stream sink, unlike `pending`'s one-shot.
- `open_tail(machine_id) -> (request_id, mpsc::Receiver<LogChunk>)`,
  `close_tail(&request_id)`, and `deliver_chunk(request_id, machine_id, chunk)`
  — the last **scoped to the machine the tail was opened against**, exactly as
  `complete()` is for command results, so one authenticated agent cannot inject
  into another's stream.
- `send_log_start` / `send_log_stop` mirror `send_command`, assigning a fresh
  non-zero `stream_id`.

### `grpc.rs`

- `LogChunk` arm → `hub.deliver_chunk(...)`; an `eof` chunk closes the sink.
  Deliberately does **not** `touch_last_seen` — a log tail is not evidence of
  agent liveness in the way a heartbeat is, and treating it as such would let a
  busy log mask a wedged agent.

### `http.rs`

- `GET /api/machines/{id}/logs/stream?source=&tail=&follow=` → `Sse<...>`:
  1. Parse and validate `source`; clamp `tail`; `400` on either failure.
  2. `409` if the machine is not in `conns`.
  3. Audit `logs.open`.
  4. `hub.open_tail` → `hub.send_log_start`.
  5. Stream chunks as SSE `data:` events, with a keep-alive so intermediaries
     don't idle the connection out.
  6. **On drop** (client disconnect, navigation, network loss) a guard sends
     `LogTailStop` and calls `close_tail`. This is the piece that stops a
     `journalctl -f` from outliving the tab that asked for it.

## Frontend

Built on **`@melloware/react-logviewer`** (`LazyLog`, MPL-2.0) rather than a
hand-rolled list. It supplies the parts that are genuinely hard — virtualization
via Virtua, search with navigation, follow/auto-scroll, line highlighting, and
the `EventSource` lifecycle including reconnect — and it does so without forcing
us off the structured NDJSON decision, because two of its props keep rendering
in our hands:

- `eventsourceOptions.formatMessage(message) => string` maps one NDJSON payload
  to a display line.
- `formatPart(text) => ReactNode` renders that line as **our own JSX**, so
  severity colour comes from the design tokens rather than ANSI escapes.

The seam between them is worth naming because it constrains both halves:
`formatMessage` must return a `string`, and `formatPart` receives only that
string — not the original object. Severity therefore has to survive as text, so
`formatMessage` emits a fixed-width level token as a prefix and `formatPart`
parses it back off. Both halves are pure and live in `lib/logs.ts`, so the
round-trip is directly testable.

**Verified before adoption:** the exact prop set above typechecks against React
19 (the package's peer range only claims `>=17`, and `LazyLog` is a class
component using `getDerivedStateFromProps`).

- `api.ts`: `LogLine` type, `logStreamUrl(id, source, tail, follow)`.
- `components/LogViewer.tsx`: a thin wrapper configuring `LazyLog` — `url` +
  `eventsource`, `follow`, `enableSearch`, and the two format callbacks. No
  hand-rolled scroll handling, and **no client-side ring buffer**: capping lines
  was only needed because an unvirtualized list would have rendered every row,
  and `LazyLog` is built for logs far larger than anything a tail will produce.
- `components/LogDrawer.tsx`: rnui `Drawer` wrapping `LogViewer`, opened from a
  per-row **Logs** action on both the Units and Containers tables. The open
  source lives in the URL (`?logs=journal:nginx.service`) so it survives reload
  and is linkable, matching the `?tab=` convention.
- A full-page route `/machines/:id/logs?source=journal:nginx.service` rendering
  the same `LogViewer`, with an expand control in the drawer navigating to it and
  a back-link returning to the machine page. Note the two surfaces use different
  parameter names on purpose: `?logs=` is an *overlay on the machine page*, while
  `?source=` is the full-page route's own subject. Sharing one name would make
  the machine page's URL ambiguous about which surface should render.
- `lib/logs.ts`: pure helpers — NDJSON → display line (`formatMessage`'s body),
  display line → parts (`formatPart`'s parser), and level → tone mapping.
  Exported so they are testable once a runner exists.

**Theming is deliberately deferred.** `LazyLog` ships CSS modules with a
dark-terminal default, which is already close to the asset-tag look. This slice
adopts that default and does not attempt to drive it from the design tokens;
matching it to the palette (and to light mode, where a dark terminal will look
out of place) is a follow-up. Called out so a reviewer reads the unstyled
viewer as a decision rather than an omission.

## Testing

- **Agent:** pure tests for `parse_source` (valid, rejected charset, unknown
  scheme), `journal_line` (full record, missing PRIORITY, array-form MESSAGE,
  malformed JSON), and `Batcher` (flush on threshold, drop accounting, marker
  emission). Live-bus/daemon tailing gated `#[ignore]`.
- **Server:** `hub` tests for the tail registry (open/deliver/close, and that a
  foreign machine cannot deliver into another's tail); a `handle_agent_frame`
  seam test that a `LogChunk` reaches its sink and `eof` closes it; `http`
  tests for `400` on a bad source and an over-long tail, `409` when the agent is
  offline, a successful open emitting SSE frames from a fake agent, and that
  dropping the response sends `LogTailStop`.
- **Frontend:** no test runner in this repo (unchanged for this slice); verified
  by `typecheck` + `build` + manual E2E, with the pure helpers exported for
  later.

## Out of scope / deferred

- Log search, filtering by level, and time-range queries — the viewer shows a
  live tail only.
- Persisting or shipping logs anywhere (PRD "forever" non-goal).
- Downloading a log to a file.
- Multi-source or fleet-wide tailing.
- Dedupe across an `EventSource` reconnect.
- OIDC actor identity on the `logs.open` audit row — lands with the browser-auth
  slice, as with every prior verb.
