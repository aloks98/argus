# Mobile / responsive pass — design

Slice 2 of the QoL pair (slice 1 = fleet identity, PR #14). Pure frontend:
no schema, no proto, no server routes — the one server-adjacent artifact is
static files that rust-embed picks up with the rest of `dist/`.

## Purpose and scope (locked during the QoL brainstorm)

**Check + simple verbs from a phone.** Fleet status, machine detail,
metrics, logs, and tapping start/stop/restart must work well at phone width.
**Terminal is explicitly NOT a mobile target** — the tab stays reachable
(someone with a bluetooth keyboard may want it) but gets zero mobile work.
**PWA manifest without a service worker**: install-to-home-screen with a
proper icon; no offline, no push.

## Grounding: what a 390×844 survey actually found (2026-07-29)

Screenshotted every surface headless at iPhone width before designing.
Findings, so this slice fixes real gaps rather than imagined ones:

- **No page-level horizontal overflow anywhere** — the table containers
  scroll internally (rnui's `table-container` is `overflow-x-auto`), the
  sidebar already collapses to an off-canvas sheet with a working trigger,
  the detail page (header, chips, spec strip, tabs, uPlot charts) stacks
  correctly, and the enroll page is already good.
- **Fleet table**: only Name/Status/IP/OS visible; CPU, trends, and
  last-seen are behind a sideways scroll — the "check" surface fails its
  purpose on the phone.
- **Units/Containers tables**: the Actions column — the whole point of
  "simple verbs" — sits off-screen right; every verb needs a sideways
  scroll first.

## Changes

### 1. Fleet: card list below `md` (user decision)

Below the `md` breakpoint the table swaps for stacked cards (`md:hidden`
list + `hidden md:block` table — the desktop table is untouched). Each card:

- display name (AssetTag treatment) with hostname beneath when renamed;
- StatusBadge + the reconnecting hint + failed-units badge (same logic the
  table's StatusCell uses — extract/reuse, don't duplicate);
- CPU and Mem as number + sparkline side by side;
- last seen, relative;
- the whole card is one tap target → the machine page.

Filtering, tag chips, search, and the Group-by dropdown are unchanged (the
filter bar already wraps at 390px); in grouped mode the cards render under
the same section headers the table rows do. All list logic stays in
`lib/fleet.ts` — the cards are a second renderer over the same
`visibleFleet`/`groupFleet` output.

### 2. Units / Containers: verbs reachable without scrolling

At `< md`, hide the low-value columns so Name / state / Actions fit at
390px: Units drops **Sub** and **Description**; Containers drops **Image**
(and the textual status if needed). `hidden md:table-cell` on both `TableHead`
and `TableCell` — the table element stays one component, no mobile fork.
The Name column's existing `max-w-[30ch]` cap may need a tighter small-screen
cap so three columns genuinely fit; exact value is the plan's to measure.
Verb buttons keep their fixed-position-per-row rule (that invariant is
documented in UnitsCard and must survive).

### 3. PWA manifest + icons

- `frontend/public/manifest.webmanifest`: name/short_name "Argus",
  `display: standalone`, `background_color`/`theme_color` `#000000`,
  `start_url: /`, icons 192/512 plus a maskable 512.
- Icons: hazard-yellow Archivo Black "A" on black, square corners — the
  asset-tag identity. Generated once (headless-browser render of an HTML
  swatch is fine) and committed as static PNGs in `frontend/public/`; no
  build-time generation, no new dependency.
- `frontend/index.html`: `<link rel="manifest">`,
  `<meta name="theme-color" content="#000000">`, `<link rel="apple-touch-icon">`
  (iOS ignores the manifest for icons), and a `<link rel="icon">` upgrade if
  the current favicon is the Vite default.
- Vite copies `public/` into `dist/` verbatim; rust-embed then serves it —
  verify `/manifest.webmanifest` returns 200 with the right MIME from the
  embedded build, since the axum static handler's fallback logic decides
  content types (survey item for the plan, not assumed).
- **No service worker.** Installability without offline is the deliberate
  scope; browsers install fine without one (no fetch handler required).
- **Secure-context caveat, stated upfront:** Chrome only offers PWA install
  on HTTPS origins (or localhost). The dev origin is plain-HTTP LAN, so the
  full install prompt appears only once Argus sits behind the production
  Traefik/cert-manager HTTPS entrypoint (PRD §2.4). iOS "Add to Home
  Screen" works over HTTP regardless and uses the apple-touch-icon. The
  dev-side check is therefore: manifest/icons served correctly + iOS-style
  manual add — not a Chrome install prompt.

### 4. Verify-only items (no code unless the check fails)

At 390px, in both themes: the three dialogs (identity edit, mint form,
token-result), the log viewer (LazyLog scroll + filter bar), the sign-in
page, and the command palette. These looked fine or weren't exercised in
the survey; the plan carries them as explicit browser-check steps, not
changes.

## Out of scope

Terminal mobile work; service worker / offline / push (ntfy covers
notifications when its slice lands); log-viewer rework; any server change
beyond static assets; landscape-tablet tuning (portrait phone + desktop are
the two supported shapes).

## Testing

No new pure logic (cards reuse `lib/fleet.ts` outputs verbatim), so this
slice is verified visually: headless phone-width screenshots of every
surface in both themes (the playwright approach from the fleet slice),
an installability check (manifest + icons fetch 200 from the embedded
build, manifest parses), and the real thing — install to your phone's home
screen from the LAN origin and tap through fleet → machine → restart a
unit.
