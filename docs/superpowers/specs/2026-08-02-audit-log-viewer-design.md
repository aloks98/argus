# Audit log viewer — design

Roadmap item 2 ("small, may ride along"). A read-only, filterable view over the
`audit_log` table that has been written to since the Spine slice — every verb,
auth event, and enrollment action, surfaced instead of `psql`-only.

## Goal

One global `/audit` page (sidebar entry) listing audit rows newest-first with
lean filters, light humanization via a static event map, a 10s head-page poll,
and keyset "Load older" paging. The machine detail page deep-links to
`/audit?machine=<id>`. No migration, no proto change, no new audit writes
(DB-only GETs are not audited anywhere in Argus — `logs.open` is audited
because it drives an agent; this endpoint drives nothing).

## Non-goals

- Per-action English sentence templates beyond the short label map (can layer
  on later without API changes).
- Live push (SSE). Audit is forensic; the 10s poll matches the app idiom.
- CSV/export, retention configuration (365d prune already exists in `jobs.rs`).
- Auditing reads of the audit log.

## API

`GET /api/audit` on the protected `/api` sub-router (session auth inherited
from the existing middleware layer).

Query params, all optional, 400 on anything unrecognized:

| param | values | default |
|---|---|---|
| `category` | `agent` \| `auth` \| `container` \| `unit` \| `logs` \| `terminal` \| `enroll_token` \| `machine` \| `local_admin` | none (all) |
| `machine` | machine UUID | none |
| `result` | `ok` \| `error` \| `denied` | none |
| `window` | `24h` \| `7d` \| `30d` \| `all` | `7d` |
| `before_id` | i64 keyset cursor (exclusive; rows with `id <` it) | none (newest) |
| `limit` | 1–500 | 100 |

Response `{ rows: AuditRow[], has_more: bool }`, where `AuditRow =
{ id, ts (RFC-3339), actor, action, machine_id, hostname, target_ref, result,
detail }`. `hostname` is nullable (fleet-level events carry no machine);
`has_more` via fetching `limit + 1`.

## Repo

One new compile-time-checked query, `repo::audit_page()`, in the established
`($n IS NULL OR col = $n)` pattern:

```sql
SELECT a.id, a.ts, a.actor, a.action, a.machine_id, m.hostname,
       a.target_ref, a.result, a.detail
FROM audit_log a LEFT JOIN machines m ON m.id = a.machine_id
WHERE ($1::text        IS NULL OR a.action LIKE $1 || '.%')
  AND ($2::uuid        IS NULL OR a.machine_id = $2)
  AND ($3::text        IS NULL OR a.result = $3)
  AND ($4::timestamptz IS NULL OR a.ts >= $4)
  AND ($5::bigint      IS NULL OR a.id < $5)
ORDER BY a.id DESC
LIMIT $6
```

- Keyset on `id DESC`: identity column, tracks insertion order, pairs with
  `before_id` exactly.
- `window` resolves to `$4` server-side (`now() - interval`); `all` passes NULL.
- Existing indexes `audit_log(ts)` and `(machine_id, ts)` suffice at this
  table's scale (365d retention, hundreds of rows per machine-week). No
  migration.
- **Result rows mutate**: `update_command_result` UPDATEs a verb row's `result`
  seconds after insert. The head page is always freshly polled (see Data flow),
  which covers that window; older loaded pages may hold a stale `result` until
  a filter change refetches — accepted.

## Frontend

**Route & nav** — `/audit` added to `routes.tsx` (section "Fleet", icon
`ScrollText`): sidebar entry + palette destination in one edit.

**Event map** — new `frontend/src/lib/audit.ts`:
- `EVENT_LABELS: Record<string, string>` covering all 19 current actions with
  short phrases (`unit.restart` → "restarted unit", `enroll_token.create` →
  "minted enrollment token", `auth.denied` → "sign-in denied", `agent.online`
  → "agent connected", …). Unknown action → raw string fallback, so new server
  actions degrade gracefully.
- `resultTone(result: string | null): Tone` — `ok`→ok, `denied`→warn,
  `error`→fail, null/other→idle.

**Page** — `AuditPage.tsx`, existing idioms only:
- `PageHeader` ("Audit", meta "Every action Argus took or refused. Refreshes
  every 10s.").
- URL-backed filter bar (FleetPage pattern): `NativeSelect`s for category /
  result / window, plus a machine select fed from the already-cached
  `useFleet()` rows. Deep-link contract: `/audit?machine=<id>`; the machine
  detail header gains a small "Audit" outline button pointing there.
- Table columns: **Time** (relative, absolute in `title`) · **Actor** (mono;
  `system` and UUID actors muted) · **Event** (humanized label, raw action in
  `title`) · **Machine** (hostname link to `/machines/:id`, "—" when null) ·
  **Target** (mono, truncate + `pointer-coarse:` wrap like StatusName) ·
  **Result** (`StatusBadge` with `resultTone`). Rows with non-empty `detail`
  get a chevron expanding a full-width sub-row of pretty-printed JSON (mono
  `text-xs`).
- States: `EmptyState` for no-rows / no-matches, `describeError()` on failure.

**Data flow** — head query `useAudit(filters)` polls the first page every 10s.
"Load older" imperatively fetches `before_id = <oldest loaded id>` and appends
to local state; any filter change resets the tail. Older pages do not poll —
poll cost stays flat regardless of depth.

## Testing

- **Repo** (`#[sqlx::test]`): seed rows across actions/machines/results/times;
  assert each filter independently, category prefix matching (`unit` must not
  match a hypothetical `units.x`), keyset ordering/exclusivity, `has_more` at
  the boundary.
- **HTTP** (oneshot): 400s for bad `category`/`result`/`window`/`limit`/UUID;
  200 shape with `rows`+`has_more`; endpoint sits behind the auth middleware
  like every `/api` route.
- **Frontend**: `pnpm --dir frontend run typecheck` + `build` clean.
- **Manual E2E** (dev host): revoke a token → row appears within 10s; filter
  by fatman; page older past 100 rows; `detail` expander on a row that has one.

## Confirmed decisions

Global page + machine deep-link; lean filter set (category/machine/result/
window); poll-head + keyset load-older; static `EVENT_LABELS` humanization
(user-requested, covering every event); default window `7d`; limit default
100 / cap 500.
