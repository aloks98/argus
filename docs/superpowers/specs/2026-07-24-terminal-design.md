# Interactive terminal (PTY) — design

**Date:** 2026-07-24
**Build order:** slice 6 in `CLAUDE.md` — the last core PRD slice (PRD §8 #6).
**Depends on:** the spine's single mTLS `Session` (frames multiplexed by
`stream_id`), the `Hub` connection registry, and the log-tail machinery this
mirrors (`open_tail` / `deliver_chunk` / `close_tail` / `send_log_start`).

## Goal

An operator opens an interactive shell to a guest from its machine page, types
and sees output in real time, resizes it, and can maximize it to full-page — all
riding the existing single mTLS `Session`, with output that throttles its source
rather than dropping bytes or stalling any other feature on that connection.

## Scope

- **In:** a **Terminal tab** on the machine page; a **WebSocket** browser↔server
  leg; `portable-pty` on the agent; **multiple concurrent sessions** per machine
  keyed by a server-minted `session_id`; resize; **`PtyFlow` end-to-end
  backpressure**; **maximize/restore**; a **30-minute idle timeout**; a **WS
  keepalive ping** (a liveness probe — Pongs are not tracked; see the handler
  section); `terminal.open` audit.
- **Out:** session recording/playback, scrollback persistence, file
  upload/download, multi-user sharing of one session, a shell picker, an absolute
  max-lifetime cap. `PtyOpen.shell` stays in the proto for later; the browser
  sends it empty.

## Architecture

```
xterm.js ─WS(binary)─▶ server WS handler ─▶ Hub PTY registry ─▶ mTLS Session (stream_id) ─▶ agent PTY task ─▶ portable-pty ─▶ shell
   ▲                        ▲   │ PtyFlow{paused}                                    │
   └───────WS(binary)───────┘   └──────────────── back to the agent ◀───────────────┘  PtyOutput
```

One browser WebSocket ⟷ one `session_id` ⟷ one agent PTY. The **server mints the
`session_id`** (a UUID) when the WS connects; the browser never supplies one,
exactly as `open_tail` mints `request_id`.

### The invariant that shapes everything

The agent↔server transport is a **single** bidirectional gRPC `Session` stream.
Heartbeats, metrics, log chunks, and *all* PTY output for *all* sessions
multiplex on that one HTTP/2 stream via `stream_id`. HTTP/2's own flow control
therefore acts on the whole Session as a unit — if the server ever stopped
reading it to slow one terminal, the agent's writes would block and heartbeats
plus every other feature would stall with it (head-of-line blocking the entire
machine connection).

So the server **always drains** the inbound Session, and per-terminal
backpressure is done at the application level (`PtyFlow`, below).

## Proto — two additive changes

```proto
message PtyOutput { string session_id = 1; bytes data = 2; bool eof = 3; }  // + eof (mirrors LogChunk)
message PtyFlow   { string session_id = 1; bool paused = 2; }               // NEW

// ServerFrame oneof (server -> agent):  PtyFlow pty_flow = 12;   // 12 is the next free number
```

`PtyOpen` / `PtyInput` / `PtyResize` / `PtyClose` are already declared and wired
into both oneofs; no change to them. We are free to change the proto in this
development stage, but these remain additive — new frame, new field numbers, no
renumbering.

### Frame mapping

| Direction | Trigger | Frame |
|---|---|---|
| browser → agent | WS opens | `PtyOpen{session_id, cols, rows, shell:""}` |
| browser → agent | keystrokes | `PtyInput{session_id, data}` |
| browser → agent | fit resize | `PtyResize{session_id, cols, rows}` |
| browser → agent | WS closes | `PtyClose{session_id}` |
| agent → browser | shell output | `PtyOutput{session_id, data}` |
| agent → browser | shell exits | `PtyOutput{session_id, eof:true}` (+ optional exit notice as `data`) |
| **server → agent** | browser buffer high/low water | **`PtyFlow{session_id, paused}`** |

## Server

### Hub — a PTY registry mirroring the tail machinery

New in-memory map `session_id → PtySession { tx, buffered_bytes: AtomicUsize,
paused: AtomicBool }`, plus methods paralleling the log-tail ones:

- `open_pty(machine_id) -> (session_id, rx)` — mints the UUID `session_id`,
  allocates a `stream_id`, registers a **bounded** `mpsc` channel, returns `rx`
  to the WS handler.
- `send_pty_open / _input / _resize / _close / _flow(machine_id, session_id, …)`
  — each sends the matching frame down the Session on that session's `stream_id`,
  via the existing non-blocking `try_send` on the outbound Session sender.
- `deliver_pty_output(session_id, data)` — called from the gRPC inbound drain.
- `close_pty(session_id)` — teardown on WS drop.

### Flow control — the careful part

The bounded channel is the buffer; `buffered_bytes` and `paused` drive the water
marks. Both edges are handled where the counter mutates, so detection works
regardless of which task is running:

- **`deliver_pty_output`** (inbound-drain task — must never block): add `len` to
  `buffered_bytes`, then `try_send`.
  - On success, if it crossed the **high-water** mark and `paused` was false →
    CAS `paused` true and `send_pty_flow(paused:true)`.
  - On a **full** channel: do **not** drop and do **not** block — `send_pty_close`
    + close the WS with "output overrun." This path must be unreachable in
    practice, so that a slow consumer causes a *pause*, never a teardown.

    **The sizing that makes that true — and the mistake an earlier draft made.**
    The channel is bounded in *messages* while the water marks are in *bytes*, so
    the message cap must never bind first. An earlier version of this design
    reasoned from 64 KiB reads and set `PTY_CHANNEL_CAP = 2048` against a 1 MiB
    high-water. That was wrong: a PTY does not return 64 KiB reads. Measured
    against a real firehose (`seq 1 200000`), chunks average **~138 bytes**
    (1,488,985 bytes over 10,818 frames), so 2048 messages filled at ~275 KiB
    while the byte watermark needed ~7,618 — the channel filled **3.7× earlier**,
    `PtyFlow` could never fire, and the "unreachable" teardown was the *first*
    branch reached. It killed any browser session running a firehose.

    The binding rule is therefore: **`PTY_CHANNEL_CAP × (smallest sustained chunk)
    must exceed `PTY_HIGH_WATER`.** With `PTY_CHANNEL_CAP = 32768` and a 1 MiB
    high-water, the breakeven is `1 MiB / 32768 = 32 bytes` per chunk — below that
    sustained average the message cap binds again. Measured ~138-byte chunks sit
    **4.3× above** that floor. Any change to these constants must re-check that
    inequality, not just "make the numbers bigger."

    A throughput test cannot verify this: a client that drains as fast as it can
    never fills the buffer. **The test is a deliberately slow consumer** — see
    `docs/DEV.md`.
- **WS output loop** (the slow point — awaits the socket): `recv` → subtract
  `len` from `buffered_bytes` → `ws.send(Binary)`. After the subtract, if it fell
  below the **low-water** mark and `paused` was true → CAS false and
  `send_pty_flow(paused:false)`.

The single gRPC stream is ordered and reliable, so pause/resume can't be lost or
reordered; the `AtomicBool` CAS suppresses duplicate signals. High-water ≠
low-water gives hysteresis against pause↔resume thrashing.

Constants (in `argus_common`, tunable): `PTY_CHANNEL_CAP`, `PTY_HIGH_WATER`,
`PTY_LOW_WATER`. Capacity must satisfy the message-vs-byte inequality above; the
high/low gap gives hysteresis against pause↔resume thrashing.

### WebSocket handler — `GET /api/machines/{id}/terminal`

On the **browser** surface, upgraded to a WebSocket (`axum`'s `ws` feature is
enabled — it is not today). Ordering mirrors the fixed `logs.open`:

1. `open_pty` + `send_pty_open` at a **default 80×24** the instant the WS
   connects (so the shell starts and the audit is clean). If the agent is offline
   → close the WS with a reason, **no audit row** (no shell was opened).
2. On dispatch success → audit `terminal.open` / `ok` (fail-closed),
   `target_ref = session_id`.
3. Run three concurrent tasks until any ends, then `send_pty_close` +
   `close_pty`:
   - **inbound WS:** `Binary` frames = keystrokes → `PtyInput`, and each resets
     the idle timer; `Text` frames = a JSON control channel
     (`{"resize":{"cols":C,"rows":R}}`) → `PtyResize`. Data and control are split
     cleanly by frame type.
   - **outbound:** the flow-controlled loop above.
   - **keepalive + idle:** a periodic WS `Ping`. NOTE: Pongs are **not** tracked,
     so this is a liveness probe rather than a detector — a dead peer is noticed
     only when a socket write eventually errors, which on a half-open TCP
     connection can take ~15 minutes. Tracking Pongs to close within ~a minute is
     a tracked follow-up; the 30-minute idle timer below is today's real backstop. An **idle timer** reset only by inbound `PtyInput`; after
     **30 minutes** with no keystroke ⇒ close the WS with "closed after 30 min
     idle." Both close paths run the same teardown.

The browser sends a resize to its real dimensions immediately after open; the
brief 80×24 is invisible before the shell draws.

### Idle timeout

`TERMINAL_IDLE_SECS = 1800` (30 min) in `argus_common`. Server-enforced,
reset on each `PtyInput`. Input-only on purpose: it answers "is a human still
driving this?", so a walked-away root shell closes even while a `top` keeps
producing output. A passive watch that outlasts the window is closed too; the
operator presses a key, or uses "Start new session."

## Agent

Modeled on `run_tail`: a `PtyOpen` spawns a task registered in
`inbound_ptys: HashMap<session_id, PtyHandle>`, torn down by `PtyClose` or
session end — the same lifecycle the log tail already has.

### Spawning

`portable-pty`'s `native_pty_system().openpty(PtySize{rows, cols, ..})`; resolve
the shell (`PtyOpen.shell` if non-empty → `/bin/bash` → `/bin/sh`); a
`CommandBuilder` with `TERM=xterm-256color` and a basic env; `slave.spawn_command()`;
then **drop the slave handle** so EOF propagates when the child exits. Keep the
master's writer and resize handle.

### The reader thread + pause (the crux of backpressure)

`portable-pty`'s reader is **blocking** (`std::io::Read`), so a dedicated
`std::thread` runs the read loop and forwards bytes via `tokio`
`mpsc::blocking_send` as `PtyOutput`. Pause is a shared
`Arc<(Mutex<bool>, Condvar)>` the thread checks **before each `read()`**:

```
loop {
  { let mut p = lock; while *p { cvar.wait(&mut p) } }   // park while paused
  let n = reader.read(&mut buf)?;
  if n == 0 { send PtyOutput{eof:true}; break; }          // shell exited
  send PtyOutput{ data: buf[..n] };
}
```

Parking *before* the read is what makes it real backpressure: a flooding producer
keeps `read()` returning data, so on `PtyFlow{paused}` the thread stops reading,
the kernel PTY buffer fills, and the writing process blocks — throttled at the
true source. An empty-buffer read merely waiting isn't consuming anything, so
pausing it costs nothing.

### Routing the other frames (on the session's async dispatch)

- `PtyInput` → blocking write to the master writer (keystrokes are tiny).
- `PtyResize` → `master.resize(PtySize{..})`.
- `PtyFlow{paused}` → set the flag + `notify` the condvar.
- `PtyClose` → `child.kill()`, drop the master (reader hits EOF and joins), remove
  from the map.

### Teardown — symmetric

- Browser closes the WS → server sends `PtyClose` → agent kills the shell.
- Shell exits → reader EOF → agent sends `PtyOutput{eof:true}` → server closes the
  WS.
- Session ends (agent redeploy/reconnect) → the existing teardown drain kills all
  live PTYs, same as tails. Matches the PRD's "sessions drop on redeploy."

`portable-pty` is the per-slice agent dependency, added now; the CI `agent-musl`
job verifies the static build still links (`portable-pty` is a Unix `openpty` +
`ioctl` wrapper, no OpenSSL/cmake).

## Frontend

Dependencies: `@xterm/xterm` + `@xterm/addon-fit`. A `TerminalView` component
rendered in the Terminal `TabsContent`.

- **On mount:** create `Terminal`, load `FitAddon`, open
  `WS /api/machines/:id/terminal`. `term.onData(s → ws.send(binary))` for
  keystrokes; `ws.onmessage` `Binary` → `term.write(Uint8Array)`; a
  `ResizeObserver` + `fit()` → the JSON resize control. The first action after
  open is a resize to real dimensions.
- **Maximize:** a button toggling a `maximized` state that **only changes the
  wrapper's layout** — `fixed inset-0 z-50` full-viewport vs the in-tab box — on
  the **same mounted** `Terminal` and WS. It must never remount (that would kill
  the session). After toggling: `fit()` + send a resize. **The button is the only
  way to restore — deliberately not Esc.** An earlier draft of this design said
  "Esc or the button", and it was implemented that way; but a key handler that
  restores on Escape must intercept it, and Escape is load-bearing inside a
  terminal (vim, less, fzf, readline). Stealing it made a maximized terminal
  unusable for exactly the full-screen programs most worth maximizing. Escape now
  always reaches the shell.
- **Teardown:** unmount (tab switch / navigate) → `ws.close()` (→ server
  `PtyClose` → agent kills the shell) + `term.dispose()`.
- **No auto-reconnect** (unlike the log `EventSource`): a shell is stateful, and
  silently reconnecting would hand the operator a *fresh* shell pretending to be
  the old one. On close, the terminal shows the reason and a **"Start new
  session"** button that opens a new one explicitly.
- **No capability gate:** the Terminal tab is always enabled. A host with no
  spawnable shell surfaces as a `PtyOutput{eof:true}` error notice in the terminal
  itself, which is cheaper and more honest than a per-session shell probe.

## Error handling — each failure surfaces *in* the terminal

- Agent offline at open → WS closes with a reason, **no audit row**, terminal
  shows "agent not connected."
- No spawnable shell → agent sends `PtyOutput{eof:true}` with an error notice →
  shown, then closed.
- Output overrun (a burst outran flow control) → server tears that one session
  down → "terminal closed: output overrun." Rare by construction.
- Agent disconnect mid-session → teardown → "session closed (agent
  disconnected)."
- Idle 30 min → "closed after 30 min idle."
- Dead socket (crashed tab / slept laptop) → the socket write eventually errors,
  or failing that the 30-minute idle timer fires → closed; the agent's
  shell is killed via `PtyClose`, so nothing leaks.

## Security posture

The terminal is unauthenticated, root-capable remote command execution. This is
the **same posture as every other browser surface today** — the browser surface
is trusted-LAN-only per the PRD — and it is audited as `terminal.open` (who
opened a shell to which machine, when). The 30-minute idle timeout is the
cheapest meaningful mitigation for the "walked-away root shell" case while there
is no auth to re-challenge. This is a conscious acceptance for the development /
trusted-LAN stage.

**OIDC is planned as its own slice, and this terminal does not build bespoke
auth.** Authentication is cross-cutting: it must gate the *entire* browser
surface (fleet, metrics, docker/systemd, logs, terminal) uniformly, so
terminal-only auth added now would be thrown away the moment OIDC lands. The
terminal ships with the posture above; the OIDC slice will authenticate every
browser surface at once, the terminal included, with no change to this design's
transport or lifecycle.

## Testing

- **Agent (unit):** shell resolution (requested → bash → sh); `PtySize` mapping.
- **Agent (`live_*`, real host only):** spawn `/bin/sh`, write `echo hi\n`, read
  `hi`; named `live_*` so CI's `--skip live_` excludes it.
- **Server (unit):** the Hub PTY registry, and above all the **flow-control state
  machine** — `buffered_bytes` crossing high-water emits `PtyFlow{paused:true}`,
  dropping below low-water emits `{paused:false}`, and a full channel tears the
  session down (no drop, no block). Idle-timer expiry closes the session.
- **Frontend:** no runner — typecheck + build, then a browser pass.
- **Live E2E:** open a shell to `fatman`; `ls`, `top` (redraw), resize,
  maximize/restore keeping the **same** session; a `yes` firehose that
  **throttles** (a `PtyFlow` pause is observed) and never drops bytes and never
  stalls heartbeats; idle close after the window; the `terminal.open` audit row.

Waiting out a 30-minute idle in an automated test is impractical, so the timeout
value is injected/shortened in tests and the *mechanism* is pinned rather than the
wall-clock.

## Out of scope / deferred

- **Tracking WebSocket Pongs.** The handler sends periodic Pings but does not
  track the replies, so a dead peer (crashed tab, slept laptop) is only noticed
  when a socket write eventually errors — on a half-open TCP connection the
  kernel can take ~15 minutes. Nothing leaks unboundedly (the 30-minute idle
  timer is the backstop), but for an unauthenticated root shell the difference
  between ~60 seconds and 30 minutes of a forgotten live session is worth
  closing. Small change; deliberately not made in the slice that discovered it.
- **Agent-side read coalescing.** The sizing fix above makes the byte watermark
  binding, but coalescing small PTY reads would raise the effective chunk size
  and widen the margin further. `portable-pty` 0.9 exposes no non-blocking peek
  on a cloned reader handle, so this needs a different read strategy.
- **Bounded PTY input queue.** The agent's writer thread is fed by an unbounded
  channel; a client that keeps sending input while a program is wedged grows
  memory rather than backpressuring. Bounded in practice by human/paste-sized
  input.

- OIDC / per-terminal auth (deferred project-wide; covers this uniformly later).
- Session recording, scrollback persistence, file transfer, session sharing.
- A shell picker (`PtyOpen.shell` stays in the wire for it).
- An absolute max-lifetime cap (idle + keepalive cover the real cases).
- Agent self-update (`UpdateAgent`, already in the proto; a separate late slice).
