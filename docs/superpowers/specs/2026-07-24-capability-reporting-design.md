# Agent capability reporting (+ the deferred log-window fix) — design

**Date:** 2026-07-24
**Build order:** follow-up to the log slices. Not a numbered PRD slice — the PRD
says nothing about capabilities or graceful degradation.
**Depends on:** the spine (`AgentInfo` on `Enroll`/`Hello`), the docker and
systemd slices (both clients already hold `inner: Option<…>`), and the full
journal slice (PR #8, merged `26c692e`) whose deferred window bug is fixed here.

## Goal

Two things, in one branch, in this order:

1. **The window fix first.** PR #8 shipped with a known bug: the time window is
   re-resolved on every request, so the tail and its pages do not share one
   cutoff. Clear that debt before adding anything on top.
2. **Capability reporting.** The agent reports which subsystems it actually has,
   the control plane stores them, and the UI disables the surfaces a host cannot
   support — instead of showing a tab that silently fails.

## Why this matters

There is currently **no capability gating anywhere** — no `cfg(target_os)`, no
flags, no UI conditioning. On a guest without systemd (Alpine is common for
Proxmox LXCs) the Units tab and every journal source fail or sit empty with no
explanation. The agent already *knows*: `SystemdClient` and `DockerClient` both
hold `inner: Option<…>` and log "unit features disabled" / "container features
disabled". It simply never tells the server.

---

# Part 1 — the log window fix

## The bug

`resolve_log_filters` recomputes `now` on **every** HTTP request, and the client
sends `window=1h`, not a resolved epoch. So each page read uses a slightly newer
cutoff than the tail that produced its anchor. Once the view has been open longer
than its own window, every page read is fully truncated by `finalize_page`,
returns empty, and the viewer reports "— beginning of window —" while visibly
displaying a longer span.

With `window=1h` this bites after roughly **an hour** of the tab being open. That
is far more reachable than "a long-open view", which is how it was characterised
when it was deferred.

## The fix

The tail and all of its pages must share **one** cutoff, and the server must be
the one to resolve it. The client cannot: a browser clock skewed against the host
would silently shift the window, and the timestamps being compared are journal
timestamps from the host's clock.

- The SSE stream emits a leading **`event: meta`** frame carrying the resolved
  `{"since_ms": <n>}` (`0` when no window is set).
- The client stores that value and echoes `since_ms=<absolute>` on every
  subsequent page read for that stream.
- `logs/page` accepts a `since_ms` parameter which **takes precedence over
  `window`** when present.

`window` stays the UI-facing concept; `since_ms` is the resolved wire value.

**Which endpoint takes which** (the two are not interchangeable):

| Endpoint | Accepts | Rationale |
|---|---|---|
| `logs/stream` | `window` only | Opening a stream *is* the act of establishing a window, so it resolves one and announces it |
| `logs/page` | `window` **and** `since_ms`, the latter winning | A page must be able to say "use the cutoff my stream was given", but still works without it |

**The server re-resolves on reconnect; the client does not re-adopt.** The
browser's `EventSource` auto-reconnects on a dropped connection — routine, since
the server ends the SSE stream whenever an agent session tears down — and the
server resolves a fresh cutoff and emits a new `meta` frame for that reconnect,
because it has no memory of what it announced before. The client, however, only
clears its line buffer when `machineId`/`source`/`filters` change, NOT on a bare
`EventSource` reconnect. So a client that adopted every announced cutoff would
pair a newer cutoff with a buffer still holding older lines: `loadOlder` would
send a cutoff newer than its own anchor, the page would truncate to nothing, and
the viewer would report "beginning of window" while visibly showing an older
span — the exact symptom this fix removes. The client therefore adopts the
announced cutoff only the first time for a given buffer (the first `meta` frame
after the buffer was last cleared) and discards whatever a later reconnect
announces. The cutoff's lifetime is pinned to the buffer's lifetime, not to the
underlying connection's — "one cutoff per buffer", not "one cutoff per TCP
connection".

Validation: `since_ms` must parse as a non-negative integer, else `400` —
consistent with the existing `priority`/`window` validation.

---

# Part 2 — capability reporting

## Wire

One additive proto field on `AgentInfo`, which already rides both `Enroll` and
`Hello`, so capabilities re-report on every session open rather than only at
enrolment:

```proto
message AgentInfo {
  string hostname       = 1;
  string machine_id     = 2;
  string os             = 3;
  string kernel         = 4;
  string primary_ip     = 5;
  string arch           = 6;
  string agent_version  = 7;
  repeated string capabilities = 8;  // NEW: "systemd" | "docker" | "journal"
  bool capabilities_reported   = 9;  // NEW: true = field 8 is authoritative
}
```

**Why field 9 exists.** proto3 cannot distinguish an *empty* repeated field from
an *absent* one — both decode to an empty vec. So a pre-capability agent and a
capability-aware agent on a bare host (no systemd, no docker, no journal — an
Alpine LXC is exactly that) are indistinguishable from field 8 alone. Since those
two cases must produce **opposite** UI behaviour (gate nothing vs gate
everything), the distinction has to be carried explicitly:

| `capabilities_reported` | `capabilities` | Stored | UI |
|---|---|---|---|
| `false` | `[]` | `NULL` | gate nothing (old agent) |
| `true` | `[]` | `{}` | gate everything (bare host) |
| `true` | `{systemd,journal}` | `{systemd,journal}` | gate docker |

The alternative — having every agent always emit one guaranteed-true sentinel
capability so "empty" implies "old agent" — works, but makes the wire format
depend on a non-obvious invariant. An explicit boolean costs one proto field and
needs no reasoning.

A **set of strings**, not typed booleans, so a future capability (terminal is the
next slice) costs no proto field and no migration. The one real cost of a
stringly-typed set — drift between writer and reader — is paid down by putting
the names in `argus-common` as constants (`CAP_SYSTEMD`, `CAP_DOCKER`,
`CAP_JOURNAL`), which both the agent and the server import. Neither side spells a
capability as a literal.

## Store

Migration `0003` adds `capabilities text[]` to `machines`, matching the existing
`tags text[]` pattern. **Nullable on purpose** — SQL already models the tri-state
this needs:

| Value | Meaning | UI behaviour |
|---|---|---|
| `NULL` | `capabilities_reported = false` — agent predates this slice | **gate nothing** |
| `{}` | `capabilities_reported = true`, host has none | gate everything |
| `{systemd,journal}` | reported | gate docker only |

Treating "not reported" as "supports nothing" would blank every tab on a working
machine the moment an older agent connects. Absence of evidence is not evidence
of absence, and `NULL` is how to say so.

## Agent probe

One `capabilities()` function, called **once per session, immediately before
`Hello`**:

| Capability | Probe | Why not something cheaper |
|---|---|---|
| `systemd` | `SystemdClient.inner.is_some()` | Already re-dialed fresh on every reconnect attempt (zbus does not self-heal), so it is current for free |
| `docker` | `docker.ping().await.is_ok()` | `Docker::connect_with_socket_defaults()` never contacts the daemon, so client construction would report "docker" on a host where dockerd is installed but **stopped**. `ping` (bollard 0.19 `system.rs`, `GET /_ping`, not feature-gated) is the only thing that proves a daemon is answering |
| `journal` | spawn `journalctl --version` | Cheap, argv-only; the binary's presence is the actual question |

**Every probe is individually timeout-bounded**, and a timeout reports the
capability *absent*. The agent's self-healing rests on reconnect being reliable;
a capability probe must never become a new way for session open to stall.

**Cost:** one D-Bus dial (already paid), one unix-socket GET, one short-lived
process — once per session, not per request.

**What `journal` claims.** `--version` proves the tooling exists, not that this
process can *read* the journal: a non-root agent gets a `journalctl` that exits 0
and prints nothing, which no exit-status check distinguishes from an empty
journal. So `journal` means "journald tooling present". A permission failure
stays a runtime concern and already surfaces as the marker line added in PR #8.
The capability claims less and means it, rather than claiming more and being
wrong.

**Refresh.** Capabilities re-report on every session open. Installing Docker
mid-session is not noticed until the next reconnect — documented, not engineered
around, because it changes approximately never.

## Server

`AgentInfo` already flows into `repo::upsert_machine` from both `Enroll` and
`Hello`, so persisting capabilities is one column write on an existing path.
`MachineDetailDto` gains `capabilities: Option<Vec<String>>`, carrying the
"never reported" case through to the client as `null`.

The fleet grid does not need capabilities (YAGNI). No new audit verb — this is
inventory, not an operator action.

## UI

`Tabs` (hand-rolled, ARIA tabs pattern) gains `disabled?: boolean` and
`reason?: string` per tab. Three details matter more than the styling:

- **Arrow-key navigation must skip disabled tabs**, or the ARIA pattern lands
  focus on a dead control.
- **The `?tab=` guard extends to disabled tabs**, falling back to `overview`. A
  bookmarked `?tab=units` for a machine that lost systemd renders the overview,
  not a blank panel — the same forgiving behaviour `?tab=typo` already gets.
- **`capabilities === null` gates nothing.**

Capability → surface mapping:

| Capability | Gates |
|---|---|
| `systemd` | Units tab |
| `docker` | Containers tab |
| `journal` | Logs tab |

A disabled tab states why ("no systemd on this host"). This follows the call
already made in the systemd slice — *show all actions for consistency, disable
whichever is not available* — so the machine page keeps the same shape on every
host and absence is explained rather than mysterious.

## Error handling

- A probe that times out or errors reports the capability absent; session open is
  never blocked.
- A capability that disappears mid-session (dockerd stopped) still shows enabled
  until the next reconnect. The verb itself already fails cleanly through the
  existing 409 / marker paths, so the failure mode is unchanged — only better
  signposted.
- An unknown capability string from a newer agent is ignored by the UI rather
  than rendered, so an older control plane degrades quietly.

## Testing

- **Agent (pure):** the probe assembles the expected set from known per-probe
  outcomes, including the all-absent and timeout cases.
- **Agent (`live_*`, real host only):** the real systemd, docker and journal
  probes against this machine. Named `live_*` so CI's `--skip live_` excludes
  them.
- **Server:** capabilities round-trip through `upsert_machine`; `None` and
  `Some(vec![])` stay distinct through the API (the tri-state is the whole
  point); a `meta` frame carries the resolved cutoff; an explicit `since_ms`
  overrides `window` on a page read.
- **Frontend:** typecheck + build, then a browser pass for the disabled tabs, the
  reason text, and the `?tab=` fallback.

Waiting an hour to observe the window symptom directly is impractical, so the
tests pin the *mechanism* (one resolved cutoff, echoed) rather than the
wall-clock symptom.

## Out of scope / deferred

- Per-capability detail beyond present/absent (e.g. systemd version, Docker API
  version).
- Capability-driven gating of the fleet grid.
- Re-probing mid-session.
- A terminal capability — that belongs with the terminal slice.
