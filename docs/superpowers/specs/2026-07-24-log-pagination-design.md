# Log load-older pagination — design

**Date:** 2026-07-24
**Build order:** follow-up to slice 5 (log tailing, PR #6 merged `978d2ae`). Not a
numbered PRD slice; an increment on the shipped bounded-tail viewer.
**Depends on:** the log tailing slice — the SSE endpoint, the agent's
`journalctl -o json` tailer, the `Hub` tail registry, and the
`@melloware/react-logviewer` viewer are all in place.

## Goal

An operator reading a systemd unit's journal can scroll up past the initial
tail and the viewer automatically loads older entries, back to the beginning of
that unit's journal — while the live tail keeps following at the bottom.

## Scope

- **In:** backward pagination for **journal** sources only, auto-triggered on
  scroll-to-top, on top of the existing live SSE tail.
- **Out:** docker pagination. Docker's log API has no cursor — only coarse
  timestamp `since`/`until`, so paging boundaries are lossy (sub-second entries
  duplicate or drop). Docker keeps the current bounded live tail with no "load
  older". This is a deliberate scope cut: journal is where scrolling back
  matters (boot logs, past failures) and is the source with an exact mechanism.
- **Out:** search *across* not-yet-loaded history, jump-to-timestamp, and
  downloading the full journal. The viewer paginates what the operator scrolls
  to; it does not index the whole journal.

## Why journald pagination is exact (and docker's isn't)

`journalctl -o json` emits an opaque `__CURSOR` per entry, and
`journalctl --cursor=<c> --reverse -n <N>` returns exactly the `N` entries
at/before that cursor with no gaps or dupes. That is the whole reason this is
journal-only: the cursor is a stable, exact backward anchor. Docker has no
equivalent.

## Proto

**One additive change** (the log slice froze the proto; this increment extends
it — additive fields only, no renumbering):

```proto
message LogTailRequest {
  string request_id   = 1;
  string source       = 2;
  uint32 tail_lines   = 3;
  bool   follow       = 4;
  string before_cursor = 5;  // NEW: page backward from this journald cursor.
                             // Empty = a normal tail from the end.
}
```

- A **live tail** is unchanged: `follow=true`, `before_cursor=""`.
- A **backward page** is `follow=false`, `before_cursor=<oldest cursor held>`,
  `tail_lines=<page size>`. The agent streams that page as `LogChunk`s then a
  `LogChunk{eof:true}` — **no new frame type**, reusing the existing stream.

The NDJSON envelope in `LogChunk.data` gains one field so the client can chain
pages:

```json
{"ts":…,"level":…,"ident":…,"msg":…,"cursor":"s=abc…;i=…;b=…"}
```

`cursor` is `null` for docker lines and for the drop/error markers (they are not
real journal entries and are never a paging anchor).

## Agent

### `logs.rs`

- `journal_record_to_line` also reads `__CURSOR` into `LogLine.cursor`
  (`Option<String>`). Existing four-field mapping is unchanged; a record with no
  `__CURSOR` (shouldn't happen for journal, but be defensive) yields
  `cursor: None`.
- `run_journal` gains the page path. When `before_cursor` is non-empty and
  `follow` is false, it runs:
  `journalctl -u <unit> --cursor <before_cursor> --reverse -n <limit+1> -o json`
  — `--reverse -n` from a cursor reads the entries *before* it, newest-first.
  The agent then **re-orders to chronological** (oldest-first) before batching,
  so the client always receives lines in display order.
  - The `+1` and the fact that `--cursor` is inclusive of the anchor entry mean
    the anchor line is fetched and **dropped** (the client already has it), so a
    page never duplicates the boundary line.
  - The agent simply streams however many entries exist (fewer than `limit` when
    the read runs into the start of the journal) then a `LogChunk{eof:true}`. It
    does **not** signal "reached start" itself — the server derives that from the
    returned line count (below), so no extra proto field is needed.
- `parse_source` and the argv-only, `kill_on_drop` spawning are unchanged; a
  page read is a short-lived non-follow process that exits on its own.

Pure functions (cursor extraction, the anchor-drop, chronological re-order) are
factored out and unit-tested without a subprocess, matching the existing
`logs.rs` test style.

## Server

The live tail endpoint (`GET …/logs/stream`, SSE) is **unchanged**.

A page is a **separate, non-SSE** endpoint:

```
GET /api/machines/{id}/logs/page?source=&before=&limit=
  → 200 { lines: LogLine[], oldest_cursor: string|null, reached_start: bool }
  → 400 invalid/docker source, or missing `before`
  → 409 agent not connected
```

- **`before` is always required.** The first backlog an operator sees comes from
  the live SSE tail (its initial `tail_lines`), not this endpoint; the page
  endpoint only ever loads entries *older than a cursor the client already
  holds*. A request without `before` is a `400`.
- Validates `source` (journal only — a `docker:` source is `400`, since docker
  pages aren't supported), clamps `limit` to `MAX_TAIL_LINES` (1000).
- Opens a short-lived tail via the existing `Hub` machinery
  (`open_tail` + `send_log_start` with `follow=false, before_cursor=before`),
  **collects** `LogChunk`s into a buffer until `eof`, then `close_tail` and
  returns JSON. This mirrors the verb path's bounded-wait pattern (a timeout
  guards a wedged agent) rather than the streaming path.
- `oldest_cursor` = the `cursor` of the first (oldest) returned line, or `null`
  when the page is empty. `reached_start = lines.len() < limit`.
- Audited as `logs.page` (a read that can expose secrets, same rationale as
  `logs.open`). The row is written *after* the tail is dispatched to the agent
  and fail-closed: an offline agent `409`s and a wedged one `504`s with **no**
  `ok` row, so the audit trail never records a read that didn't happen. On
  completion the endpoint also sends a `LogTailStop` so the agent drops its
  short-lived page tail rather than leaking a handle per fetch.

**Why a separate endpoint, not more SSE:** a page is a *discrete batch with a
next-cursor*, not a stream. Request/response models that exactly; the frontend
wants the batch and the `oldest_cursor` in one shot to decide whether to enable
another load. The transport underneath still reuses the one Session stream and
the `LogChunk` frame — only the server-side collection differs.

## Frontend — the substantive rework

`@melloware/react-logviewer`'s `eventsource` mode is **append-only** (it
`bufferConcat`s incoming data), so it cannot prepend. The viewer moves to
LazyLog's controlled **`text`** mode, and `LogViewer` owns the data:

- **Buffer + EventSource.** `LogViewer` opens the live `EventSource` itself
  (replacing LazyLog's built-in one), parses each NDJSON line, and appends to a
  `lines` buffer (a ref-backed array). The buffer is rendered by passing
  `text={lines.map(display).join("\n")}` to LazyLog.
- **Prepend on scroll-to-top.** An `onScroll` handler detects proximity to the
  top; when the operator is near it and not already fetching and `reached_start`
  is false, it calls `GET …/logs/page?before=<oldest held cursor>`, **prepends**
  the returned lines, and immediately calls LazyLog's `scrollToLine` targeting
  the line that was previously at the top (its index shifted by the prepended
  count) so the viewport does not jump.
- **Follow vs history.** Following (auto-scroll to bottom on new live lines)
  **pauses** whenever the operator is scrolled away from the bottom, and
  **resumes** when they return to the bottom — the universal log-tailer
  behaviour. New live lines still append to the buffer while paused; they are
  just not scrolled to.
- **Bounds.** A total-buffer cap (~50k lines) keeps memory bounded on a long
  session; hitting it is silent (virtualization already means only on-screen
  rows render). The per-page size is 500.
- **States.** A top affordance shows "loading older…", and a terminal
  "beginning of journal" once `reached_start`. A docker source shows neither
  (no pagination) — unchanged behaviour.

The `formatMessage`/`formatPart` seam and the dialog, selection, colour and
alignment work from the log slice are preserved — only the data source flips
from LazyLog's EventSource to ours.

## Error handling

- A page fetch that 409s (agent went offline mid-scroll) surfaces a dismissible
  "couldn't load older entries — agent offline" note at the top and leaves the
  loaded buffer intact; the live tail's own reconnect is independent.
- A page timeout (wedged agent) behaves like the 409 path.
- The live tail's disconnect/`LogTailStop` lifecycle is unchanged.

## Testing

- **Agent:** pure tests for `__CURSOR` extraction, the anchor-line drop
  (a page whose first record is the requested cursor omits it), and the
  reverse→chronological re-order. Live-bus paging gated `#[ignore]`.
- **Server:** `oneshot`/`sqlx::test` tests for the page endpoint — `400` on a
  docker source, `409` offline, a happy path returning `{lines, oldest_cursor,
  reached_start}` from a fake agent that streams a page + `eof`, `reached_start`
  true when the page is short, and the `logs.page` audit row.
- **Frontend:** no runner (unchanged); the buffer/prepend/scroll-anchor logic is
  factored into pure helpers in `lib/logs.ts` (prepend + index-shift math) so it
  is testable later; verified by typecheck + build + manual E2E in a **browser**
  (the log slice's Critical was a curl-only miss — a real browser is required for
  the scroll-anchor and follow-pause behaviour).

## Out of scope / deferred

- Docker pagination (timestamp-based, lossy).
- Search / jump across unloaded history.
- Persisting or downloading the full journal.
- A "jump to now" button (resuming follow by scrolling to bottom covers it).
