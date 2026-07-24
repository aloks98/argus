# Full system journal + log filters — design

**Date:** 2026-07-24
**Build order:** follow-up to the log slices (tailing, PR #6 `978d2ae`; load-older
pagination, PR #7 `44e466d`). Not a numbered PRD slice — the PRD defines only
`"docker:<container-id>" | "journal:<unit>"` (§ proto, line 303) and no filtering.
**Depends on:** the log tailing and pagination slices — the SSE endpoint, the
`logs/page` endpoint, the agent's `journalctl -o json` reader, `finalize_page`,
and the `LogViewer` in controlled `text` mode.

## Goal

An operator can read the **whole system journal** for a machine — not just one
unit — filtered by severity and time window, with the same live tail and
scroll-back-to-load-older behaviour the per-unit view already has. This is the
Cockpit "Logs" page equivalent.

## Scope

- **In:** a full-journal source; a priority filter; a time window (current boot /
  1h / 24h / all); both filters applied to **per-unit journal reads as well**; a
  new full-page **Logs tab** on the machine page.
- **Out:** docker (no cursor — unchanged from the pagination slice); server-side
  text grep across unloaded history (`journalctl -g` interacts badly with cursor
  paging); saved filter presets; cross-machine log search.
- **Out (tracked follow-up):** agent capability reporting. See "Platform support".

## Platform support

This requires **systemd/journald**. It works on Debian/Ubuntu, Fedora/RHEL, Arch,
and Flatcar (the static-musl target). It does **not** work on Alpine, Void,
Devuan, OpenWrt, or non-Linux — and Alpine is common for Proxmox LXCs, so this
will come up in a real fleet.

This slice **narrows nothing**: `journal:<unit>` already shells out to the same
binary and the systemd slice already needs D-Bus. Shelling out to `journalctl`
rather than linking libsystemd is deliberate — it is what keeps the musl static
build working.

The real gap is that there is **no capability reporting anywhere** (no
`cfg(target_os)`, no capability flags, no UI gating). On a non-systemd guest the
Units tab and journal sources fail or sit empty with no explanation. Fixing that
properly — agent reports capabilities at enrollment, UI hides or explains
unsupported surfaces — is a **separate slice**. This design takes only the cheap
half: a failed `journalctl` spawn emits a visible marker line (below).

## The journalctl constraint that shapes this design

Verified empirically against the live journal on a systemd host:

| Combination | Result |
|---|---|
| `-p <n>` + `--cursor` | composes |
| `-b` + `--cursor` | composes; paging past the boot start returns only the inclusive anchor |
| `--since` + `--cursor` | **rejected**: "Please specify only one of --since=, --cursor=, --cursor-file=, and --after-cursor=" |
| `--since @<epoch>` (no cursor) | works |
| `-b` + `--since` | accepted |

So a relative time window **cannot** be expressed as `--since` on a paginated
(cursor-anchored) read. That single fact drives the agent design below.

## Source grammar

`journal:@system` — the sentinel for "no `-u`".

`@` already passes both validators unchanged (`is_unit_char` in
`crates/agent/src/logs.rs`, `source_is_valid` in `crates/server/src/http.rs`), and
systemd rejects any unit name beginning with `@`, so it cannot be confused for
one. **No validator change is needed.**

`parse_source` maps it to a new `Source::JournalAll` variant rather than
`Source::Journal("@system")`, so "no unit" is explicit in the type system instead
of a magic string threaded through the argv builders.

## Proto — additive only

```proto
message LogTailRequest {
  string request_id    = 1;
  string source        = 2;
  uint32 tail_lines    = 3;
  bool   follow        = 4;
  string before_cursor = 5;
  uint32 max_priority  = 6;  // NEW: 0-7 syslog; 0 = unset (no -p)
  uint64 since_ms      = 7;  // NEW: absolute unix ms cutoff; 0 = unset
  bool   current_boot  = 8;  // NEW: -b
}
```

Three additive fields, no renumbering — the same pattern `before_cursor` used.

- `max_priority` is a **severity ceiling in syslog numbering, where lower is more
  severe**: it becomes `journalctl -p <n>`, which returns entries with numeric
  priority **≤ n**. So `max_priority = 3` (err) returns err, crit, alert and emerg
  — not "err and below". Do not invert this.
- `max_priority = 0` means **unset**, not "emerg only". Emerg-only is not a filter
  anyone picks, and this keeps the zero value meaning "no filter" for all three
  fields, so default-valued requests reproduce today's behaviour exactly.
- The window is **two fields, not an enum**, because `current_boot` is a flag
  (`-b`) and `since_ms` is a value (`--since @<epoch>`), and they behave
  *differently under pagination* (see the table above). An enum would hide the
  distinction the agent must act on.
- `since_ms` is **absolute** — an epoch-ms value, not a duration — so a page
  read can apply it as a plain `ts >= since_ms` comparison without needing to
  know when "now" was. It is resolved by the server fresh on every request
  (see the Server section below): the tail and each page call
  `resolve_log_filters` independently, so a long-open view's cutoff quietly
  creeps forward with each request rather than staying pinned to when the view
  was first opened. See "Deferred" for the real fix.

## Agent

### argv construction

`Source::JournalAll` and `Source::Journal(unit)` share one builder; the only
difference is whether `-u <unit>` is emitted.

| Filter | Live tail (no cursor) | Page read (cursor-anchored) |
|---|---|---|
| `max_priority` | `-p <n>` | `-p <n>` |
| `current_boot` | `-b` | `-b` |
| `since_ms` | `--since @<epoch>` | **omitted** (rejected with `--cursor`) |

### The cutoff

Because `--since` cannot ride a page read, `finalize_page` gains one step after
the existing anchor-drop and reverse: **drop entries whose `ts < since_ms`**.
Every line already carries `ts`, so this is a filter, not new plumbing.

A truncated page comes back short, so the server's existing
`reached_start = lines.len() < limit` fires unchanged — it now means "start of the
window", which is the correct semantics in both modes. **No new proto signal and
no new server logic.** The whole time-window special case stays inside one pure,
unit-testable function.

### Spawn failure

A missing `journalctl` fails at spawn. That already propagated as an `Err` out
of `run_journal`/`run_journal_page` into `run_tail`'s error arm, which pushes
its own generic `log tail ended: <error>` marker — so the operator was never
looking at a blank view. `spawn_failure_marker` (added in the pagination
fix-wave) only improves the message: a specific "journalctl could not be
started" line takes the place of the generic wrapper text, so an Alpine guest
gets a clearer diagnostic rather than a vaguer one.

## Server

- `logs/stream` and `logs/page` both gain `priority`, `since`, and `boot` query
  parameters, validated and passed through to `send_log_start`.
- **Validation:** `priority` must be 0–7; `since` must be a positive epoch-ms.
  Anything else is a `400`, consistent with existing source validation.
- **`since` is resolved to an absolute epoch by the server** at request time —
  but independently for every request. The tail and each later page call
  `resolve_log_filters` on their own, computing `now` fresh each time, so a
  view held open across a window boundary (e.g. `window=24h` for more than a
  day) ends up with each page's cutoff a little newer than the one before it,
  and the view can accumulate lines older than its own current window. This is
  not "one cutoff shared" — see "Deferred" below.

The UI's single `window` value maps onto the two proto fields as follows — this is
the only place the mapping is defined:

| `window` | `current_boot` | `since_ms` |
|---|---|---|
| `boot` | `true` | `0` (unset) |
| `1h` | `false` | `now_ms - 3_600_000` |
| `24h` | `false` | `now_ms - 86_400_000` |
| `all` | `false` | `0` (unset) |

The two are never set together: `boot` and a relative window are alternative
answers to the same question, and combining them would only narrow twice.
- **Audit:** rows keep their shape, but the `target` string records the filters
  alongside the source — a filtered read and a full read are different
  disclosures, and the audit trail should show what was actually read.
- Audit ordering is unchanged from the pagination fix-wave: dispatch first, then
  audit, then collect, so a 409/504 leaves no misleading `ok` row.

`max_priority` and `current_boot` change *which* entries exist, so page boundaries
shift between filter settings. This needs no handling: cursors are global, and the
client always pages from a cursor it currently holds.

## Frontend

### Logs tab

A 4th entry in the `TABS` array in `MachineDetailPage.tsx`
(`overview / containers / units / logs`), rendering `LogViewer` at full height
with a filter bar above it, sourced from `journal:@system`.

### Filter bar

One new `LogFilterBar` component (priority select + window select), used by
**both** the Logs tab and the per-unit dialog — one control, one code path.

### Defaults — deliberately different per surface

| Surface | Priority | Window |
|---|---|---|
| Logs tab (full journal) | all | **current boot** |
| Per-unit dialog | all | **all** (unchanged) |

The asymmetry is intentional. Defaulting the per-unit view to current-boot would
be a **regression**: `-b` composes with `--cursor`, so it would silently cap
scroll-back at the last reboot on a view that can currently page back
indefinitely. The full journal has no such history expectation and benefits from
the cheaper, more relevant default. Priority defaults to unset everywhere so that
adding filters changes no existing behaviour.

### URL state

Filters join the existing `?tab=` / `?logs=` convention, so a filtered view is
linkable and survives reload:

```
?tab=logs&priority=4&window=boot
?tab=units&logs=journal%3Anginx.service&priority=3&window=all
```

`window` is one of `boot | 1h | 24h | all`. Unknown or invalid values fall back to
the defaults rather than rendering nothing — the same guard `?tab=typo` gets.

### Filter change = new read

Changing a filter tears down the EventSource and reopens it, resetting the line
buffer and all pagination state. Mechanically this is what a source change already
does: the existing `useEffect([machineId, source])` gains the filter values as
dependencies. No new lifecycle.

The end-of-history affordance becomes window-aware: **"— beginning of window —"**
when a window is set, **"— beginning of journal —"** when it is not.

## Testing

- **Agent (pure):** argv construction per filter combination — specifically that
  `--since` is *omitted* on cursor-anchored reads while `-b`/`-p` are retained;
  cutoff truncation in `finalize_page`; `journal:@system` → `Source::JournalAll`.
- **Server:** parameter validation `400`s, correct passthrough into
  `LogTailRequest`, and that the audit `target` records the filters.
- **Live (real host only):** a filtered full-journal page read. Named `live_*` so
  CI's `--skip live_` excludes it (see `docs/DEV.md`).
- **Frontend:** typecheck + build, then a browser pass — filter change resets the
  buffer, window-aware end-of-history wording, per-unit defaults visibly
  unchanged, and the Logs tab streams and pages.

## Out of scope / deferred

- Agent capability reporting + UI gating for non-systemd guests (the real fix for
  "Platform support" above).
- Server-side text grep across unloaded history.
- Saved filter presets; cross-machine log search.
- **A truly single, pinned cutoff.** `since_ms` is resolved fresh on every
  request (see the Server section above), not shared between the tail and its
  later pages. The real fix is for the server to return the `since_ms` it
  resolved on stream open, and for the client to echo that exact value back on
  its page requests instead of re-sending `window` for the server to
  re-resolve — pinning the cutoff for the life of the view rather than letting
  it creep forward.
- Docker filtering (docker has no cursor and no priority concept).
