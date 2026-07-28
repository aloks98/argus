# Fleet identity & navigation — design

Slice 1 of the QoL pair agreed 2026-07-28 (slice 2 = the mobile/responsive
pass, separate spec). Everything here is browser-surface + one migration; no
proto change, no agent change.

## Purpose

As the fleet grows, machines need human identity and fast navigation:

- **Display names** — `fatman` is a hostname, "Media box (Jellyfin)" is what
  you think of it as. Named at enrollment or renamed later in the UI.
- **Tags** — free-form labels (`infra`, `media`, `pve1`) that filter or group
  the fleet page. Also the substrate the later bulk-exec slice targets.
- **Notes** — free text per machine ("the one with the flaky NIC").
- **Enroll page** — mint enrollment tokens in the UI (with name/tags attached),
  see the copy-paste run block. Kills the psql ritual in `docs/DEV.md`.
- **Search + command palette** — type-to-filter on the fleet page; Ctrl+K
  anywhere to jump to any machine or straight to one of its tabs.

Decisions locked during brainstorming:

1. Name/tags at enroll time ride **on the enrollment token**, applied to the
   machine when the token is used — server-side and trusted, nothing
   agent-self-reported.
2. Tags are **free-form with autocomplete** (no curated list, no management
   page). A tag exists while at least one machine carries it.
3. Fleet page offers **both flat+filter and grouped views**, user-toggled.
4. In the grouped view a machine appears **under every tag it has**
   (groups are views, not a partition), plus an "Untagged" section.
5. Out of scope, explicitly: richer fleet cards (declined), mobile pass
   (slice 2), any tag-based authorization.

## Data model (migration 0006)

`machines.tags text[]` (GIN-indexed), `machines.notes`, and the token columns
`max_uses` / `uses` / `expires_at` / `revoked` / `created_by` **already exist**
in 0001 — unused until now. The migration adds only what's missing:

```sql
alter table machines add column display_name text;
alter table enrollment_tokens add column display_name text;
alter table enrollment_tokens add column tags text[] not null default '{}';
```

`display_name` is nullable: **null means "use the hostname"**. There is no
default-to-hostname copy at enroll; the fallback lives in display logic so a
later hostname change keeps showing through for machines never renamed.

## Validation rules (one server-side implementation, shared by every write path)

- **Tags:** trim → lowercase → dedupe (order-preserving) → reject the empty
  string. Each tag must match `^[a-z0-9][a-z0-9._-]{0,31}$` (URL- and
  chip-safe; no spaces). Max **16 tags** per machine/token. Violations are a
  `400` naming the offending tag — never silent dropping.
- **display_name:** trimmed; the empty string becomes **null** (= clear back
  to hostname); max **64 chars**.
- **notes:** max **4000 chars**; stored as-is (nullable; null and empty
  render identically).

## Enrollment flow

Minting (UI or API) takes: a label (`name`, required — the audit-facing
identifier for the *token*), optional `display_name`, optional `tags`,
optional `max_uses` (**default 1**) and `expires_at` (**default now + 24h**).
UI-minted tokens are therefore single-use and short-lived unless the operator
deliberately widens them — the DEV.md forever-token becomes the exception,
not the norm.

On successful enrollment the token's identity fields are applied to the
machine row **only where set**: a non-null `display_name` overwrites, a
non-empty `tags` array overwrites, otherwise the machine's existing values
are untouched. This makes re-enrollment after CA rotation (the recovery in
DEV.md) identity-preserving by default, while still allowing a deliberate
rename-via-token. Applied on both the create and re-enroll paths of the
Enroll handler.

The raw token is returned **exactly once**, from the mint response; only the
sha256 is stored (unchanged from today).

## API (all on the authenticated `/api` sub-router; every mutation audited)

- `PATCH /api/machines/:id` — JSON body with any of `display_name`
  (string | null), `notes` (string | null), `tags` (string[]). Only provided
  fields change. Audit: `machine.update`, detail = which fields changed (not
  the values of `notes` — it may hold anything).
- `GET /api/enrollment-tokens` — active + revoked/expired tokens: `id`,
  `name`, `display_name`, `tags`, `max_uses`, `uses`, `expires_at`,
  `revoked`, `created_by`, `created_at`. Never the hash.
- `POST /api/enrollment-tokens` — mint; returns the row **plus `token`**, the
  raw secret, exactly once. `created_by` = the authenticated actor. Audit:
  `enroll_token.create` (target = token id, detail = label).
- `DELETE /api/enrollment-tokens/:id` — soft revoke (`revoked = true`), 204.
  Audit: `enroll_token.revoke`.
- `GET /api/ca.pem` — the CA certificate (`text/plain`), for the enroll
  page's download button. Public knowledge cryptographically, but kept
  behind auth like everything else on `/api`.
- Fleet + machine-detail payloads gain `display_name`, `notes`, `tags`.

No `GET /api/tags`: the tag vocabulary (for chips and autocomplete) is
derived client-side from the fleet query, which every relevant page already
has cached via TanStack Query. One source, no extra endpoint.

## UI

**Fleet page.**
- A search input filtering (client-side, case-insensitive substring) on
  display name, hostname, and tags.
- Tag chips (rnui `Badge`/`Toggle`) — clicking narrows to machines carrying
  **all** selected tags (AND semantics; OR across a homelab-sized fleet just
  reads as "everything").
- A flat ↔ grouped toggle (rnui `ToggleGroup`). Grouped renders one section
  per tag (alphabetical), machines under every tag they carry, "Untagged"
  last. Cards are the existing cards, unchanged.
- View state lives in the URL (`?q=…&tags=a,b&view=grouped`) so a view is
  shareable and survives refresh — same pattern as `useLogFilters`. Absent
  params mean: empty search, no tag filter, **flat** view.
- Cards show the display name; when it differs from the hostname, the
  hostname beneath in muted mono (same treatment the machine header uses).

**Machine detail.** In the header area: inline-editable display name, a tag
editor (rnui `Combobox` with `ComboboxChips` + autocomplete over the derived
vocabulary, free entry allowed), and a notes textarea with an explicit Save
(not save-on-blur — accidental edits to notes shouldn't persist silently).
All three call `PATCH /api/machines/:id` and invalidate the fleet/machine
queries.

**Enroll page** (`/enroll`, linked from the fleet page header).
- The mint form: label (required), display name, tags, advanced collapsible
  for `max_uses` / expiry.
- The result screen, shown once per mint: the raw token in a `CopyButton`
  block, a CA-download button (`/api/ca.pem`), and the run block —
  the `sudo -n env …` recipe from DEV.md templated with the token, with
  `ARGUS_AGENT_ENDPOINT` left as a visible `<agent-endpoint>` placeholder:
  the control plane cannot know its externally routable agent address (in
  k8s it's the MetalLB LB), and a wrong guess pasted verbatim costs more
  than an honest placeholder.
- Below: the token list with state (active / used up / expired / revoked)
  and a revoke action per row.

**Command palette.** rnui `CommandDialog`, opened by Ctrl/Cmd+K (and a
search-shaped button in the sidebar footer for discoverability). Client-side
over the cached fleet list — no new endpoint, nothing fetched on open.
Items: every machine (searchable by display name, hostname, tags) navigating
to its detail page, with per-machine sub-actions for the tabs (Overview /
Docker / Units / Logs / Terminal — capability-gated the same way the tabs
are); plus static entries (Fleet, Enroll). Matching uses `Command`'s
built-in filter.

## Audit

New actions: `machine.update`, `enroll_token.create`, `enroll_token.revoke`
— written by the same `repo::audit` path as every existing verb, actor = the
authenticated identity. (Standing rule: a verb without an audit write is
incomplete.)

## Testing

**Server (unit/integration, in-crate):**
- Tag normalization: trimming, lowercasing, dedupe order, each rejection
  class (empty, too long, bad chars, >16).
- `PATCH` partial-update semantics: absent field untouched, `null` clears
  display_name, audit row written naming changed fields.
- Enroll-time application: token with name+tags sets both on first enroll;
  token with neither leaves an existing machine's identity untouched on
  re-enroll (the CA-rotation recovery case, asserted directly).
- Mint defaults: `max_uses = 1`, `expires_at ≈ now+24h`; raw token present
  in the mint response and absent from the list response.
- Revoked/expired/used-up token rejected by the Enroll handler (extends the
  existing spine tests to the new mint path).

**Frontend:** filtering, grouping, and palette-item construction are pure
functions in `frontend/src/lib/fleet.ts` (mirroring `lib/units.ts`) so the
logic is reviewable without a DOM. Visual verification in a real browser,
both themes, per the standing lesson.

**Live E2E (recorded in DEV.md):** mint a single-use token on the enroll
page with a name+tag → enroll a fresh agent with the pasted block → machine
appears with that name and tag → second use of the token is refused →
rename + retag from the detail page → fleet filter and grouped view reflect
it → Ctrl+K jumps to the machine's Logs tab.

## Out of scope

- Mobile/responsive pass (slice 2, own spec).
- Richer fleet cards (declined during brainstorming).
- Tag-based authorization or per-tag RBAC.
- Bulk tag editing across machines (comes naturally with bulk-exec later).
- Deleting machines from the UI (unrelated to this slice).
