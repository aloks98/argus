# Frontend design system — design

**Date:** 2026-07-23
**Branch:** `frontend-design-system` (base: `main` @ `9a969e4`, after Docker slice #3)
**Scope:** the embedded React SPA (`frontend/`). No server, proto, or agent changes.

> **Superseded in places.** Several decisions below were changed after the design
> was reviewed running. See **[Amendments after live review](#amendments-after-live-review)**
> at the end — that section wins wherever the two disagree.

## Goal

Give Argus a deliberate visual identity and a shared component layer, before the
remaining slices (systemd, logs, terminal) each add another screen and multiply
the existing duplication.

Two halves, done together because they depend on each other:

1. **Identity** — a design system with real tokens, replacing ad-hoc styling.
2. **Structure** — a shared layer (hooks, components, utils) replacing copy-paste.

## Why now

The current frontend has no shared layer and no token system. Concretely:

- `formatLastSeen` is byte-identical in `FleetPage.tsx:45` and `MachineDetailPage.tsx:67`.
- The poll effect (`cancelled` flag + `setInterval` + try/catch/finally) is
  hand-rolled twice (`FleetPage.tsx:68`, `MachineDetailPage.tsx:318`).
- Status→variant mapping exists three times (`STATUS_VARIANT` in both pages, plus
  `CONTAINER_STATE_VARIANT`).
- `<main className="mx-auto max-w-5xl p-8 font-sans">` is repeated six times.
- Colours are hard-coded Tailwind grays (`text-gray-600`, `text-gray-500`) in seven
  places. **These ignore the theme toggle** — a live bug, not just inconsistency.
- `MachineDetailPage.tsx` is 526 lines doing six jobs.

## Direction: asset tag

Chosen from three bold candidates (asset tag / neo-brutalist / arcade). Its
personality derives from the subject's own world — **rack asset labels, port tags,
label-maker strips** — rather than an imported web trend, so it should still read
well after long use.

**Signature element: the asset tag.** A hostname renders as a solid stencilled tag
whose *fill is the status*. The same device operates at two scales:

- **Fleet strip** — one block per machine at the top of the fleet page. Answers
  "is anything red?" across 40 guests without reading a row.
- **Row tag** — the hostname cell in the fleet table.

The tag is deliberately ground-agnostic (solid fill, black text), so it is
identical in both themes. That is what makes light-theme support cheap.

### Structural rules

- `--radius: 0` everywhere. Sharp, no rounded corners.
- Boldness comes from **solid fills and 2px structural rules** — never hairlines.
  (A zero-radius hairline "broadsheet" is a generic default; this deliberately is
  not that.)
- Micro-labels (table headers, nav, eyebrows): uppercase, `letter-spacing: .14em`,
  10–11px.
- Full-width layout. The current `max-w-5xl` wastes screen on a 10-column table.

## Palette

Dark is the default theme. Status colours are load-bearing — they *are* the
information — so nothing else may compete with them.

### Dark (default)

| token | value | role |
|---|---|---|
| `ground` | `#000000` | page background (true black) |
| `ink` | `#F5F5F5` | primary text |
| `dim` | `#8A8A8A` | secondary text |
| `rule` | `#242424` | 2px structural rules, dividers |
| `brand` | `#FFE600` | logo block, active tab, primary button |
| `ok` | `#00E676` | running / online |
| `warn` | `#FF6D00` | degraded / reconnecting |
| `fail` | `#FF1744` | offline / failed / exited-error |

**Why warn is orange, not amber.** Brand owns hi-viz yellow. An amber warn
(`#FFC400`) is indistinguishable from it, which would make a degraded machine read
as brand chrome — a functional defect in a view whose whole job is spotting bad
rows. Warn moved to `#FF6D00` and fail to crimson `#FF1744`, preserving an
unambiguous green→orange→red severity ramp with maximum separation at
fleet-strip cell size.

### Light

Only chrome shifts. Brand and status **fills** are unchanged across themes.

| token | value |
|---|---|
| `ground` | `#FFFFFF` |
| `ink` | `#0A0A0B` |
| `dim` | `#6B6B70` |
| `rule` | `#D4D4D8` |

**Contrast rule for light mode.** The bright palette works as a *fill* (black text
on a saturated block) on either ground, but fails as *text* on white. So where a
colour is used as text, underline, or icon in light mode, it uses a darkened
variant; where it is used as a fill, it is unchanged:

| role | fill (both themes) | text (light only) |
|---|---|---|
| brand | `#FFE600` | `#8A7300` |
| ok | `#00E676` | `#007A3D` |
| warn | `#FF6D00` | `#C24E00` |
| fail | `#FF1744` | `#C4001D` |

## Type

| role | face | used for |
|---|---|---|
| display | **Archivo Black** | page titles, the brand mark — used sparingly |
| UI | **Archivo** | labels, buttons, body |
| data | **IBM Plex Mono** | all machine truth |

"Machine truth" means hostnames, IPs, container ids, image names, `Exited (2)`,
uptime, load, byte counts. The mono/sans split encodes a real distinction —
machine data vs human chrome — rather than decorating.

Faces are **self-hosted via `@fontsource`** (latin subset, only the weights used),
not a CDN: the SPA is embedded in the Rust binary and must render without external
network access.

## Shell and information architecture

```
┌──────────────────────────────────────────────────────────────┐
│ ARGUS │ Fleet  Audit  Tokens              14/16 ONLINE   ☾   │
├──────────────────────────────────────────────────────────────┤
│ FLEET                                                        │
│ ▚▚▚▚▚▚▚▚▚▚▚▚   ← fleet strip, one block per machine           │
│  MACHINE      STATUS    IP              CPU    LAST SEEN     │
│  [fatman]     online    192.168.150.40  0.9%   12s           │
└──────────────────────────────────────────────────────────────┘
```

The `STATUS` column is not redundant with the tag fill — see the accessibility
floor below. Colour reinforces the status; it never carries it alone.

Machine detail gets a sticky header plus tabs:

```
│ [fatman]                                          ONLINE     │
│ Debian 13 · 192.168.150.40 · x86_64 · agent 0.1.0            │
│ ┌ OVERVIEW │ CONTAINERS ┐                                    │
```

**Scope call:** the tab component is built, but only **Overview** and
**Containers** are wired — the tabs that have content today. Units, Logs, and
Terminal tabs arrive with their own slices rather than shipping empty shells.

## Dependencies

Added:

| package | why | approx cost |
|---|---|---|
| `@tanstack/react-query` | the whole data layer (below) | ~13KB gz |
| `class-variance-authority`, `clsx`, `tailwind-merge` | typed component variants | ~2KB gz |
| `uplot`, `uplot-react` | time-series charts (replaces echarts) | ~40KB |
| `@fontsource/archivo`, `@fontsource/ibm-plex-mono` | self-hosted faces | subset only |

Deliberately **not** added:

- A date library. `Intl.RelativeTimeFormat` is built in and covers `12s` / `3d`;
  `date-fns`/`dayjs` would be dead weight for one function.
- `@tanstack/react-table`. Columns are static today; revisit when a slice actually
  needs sorting or filtering.
- `zod`. The API is our own and typed end-to-end; runtime validation is not
  earning its bytes yet.

## Data layer

**TanStack Query owns all server state.** This replaces hand-rolled polling
outright — there is no `usePoll` hook.

- Polling is `refetchInterval` (fleet 5s, machine detail 10s), with cancellation,
  dedup, and cleanup handled by the library.
- `loading` / `error` / `notFound` `useState` triples on both pages collapse into
  the query's own `isPending` / `error`. A 404 is distinguished by the error thrown
  from `lib/api.ts` and mapped to the not-found view.
- Container verbs become `useMutation` + `invalidateQueries(['docker', id])`,
  which removes both the manual `refetchDocker()` and the `busy` Set — per-row
  in-flight state comes from the mutation itself.
- Stale-while-revalidate means a poll no longer flashes "Loading…" over live data,
  and fleet ↔ detail share one cache.

Query keys are centralised in `lib/queries.ts` so no screen invents its own.

## File structure

```
frontend/src/
  app/         AppShell, routes, QueryClient provider
  components/  AssetTag, StatusBadge, FleetStrip, PageHeader, Tabs,
               TimeSeriesChart, MetricChartCard, ContainersCard, Sparkline
  lib/         api.ts (+ request helper), queries.ts, format.ts, metrics.ts,
               status.ts (cva variants), cn.ts
  pages/       FleetPage, MachineDetailPage
```

What this removes: `formatLastSeen` ×2, the poll effect ×2, the loading/error
state triples ×2, three status-variant maps, the manual refetch + `busy` Set, the
page-shell `<main>` ×6, and the 526-line file (series transforms move to
`lib/metrics.ts`, cards to `components/`).

Component variants (`AssetTag`, `StatusBadge`) are expressed as **cva** variant
tables in `lib/status.ts`, typed so an invalid status is a compile error rather
than a silent fallback — replacing the three hand-rolled `Record<string, …>` maps
and their `?? "outline"` defaults. `cn()` (clsx + tailwind-merge) is the standard
class merge helper, matching how Rnui itself is built.

## How theming lands

Tokens install as **CSS variable overrides on Rnui's existing shadcn-style
variables** in `index.css` (`--background`, `--foreground`, `--muted-foreground`,
`--primary`, `--border`, `--radius`, …). Every Rnui component then inherits the
identity without being individually restyled, and the seven hard-coded grays are
replaced by token-driven classes — which is what fixes the theme-toggle bug.

`index.css` must keep its existing `@import "@e412/rnui-themes"` and the
`@source "../node_modules/@e412/rnui-react/dist"` directive (Tailwind v4 does not
scan `node_modules`); the token overrides layer on top of those.

## Charts and bundle size

The initial bundle is ~1.3MB and Vite already emits a >500KB chunk warning.
Echarts — pulled in by Rnui's `LineChart` — is the bulk of it.

### Measured baseline (2026-07-23)

The validation gate was run during planning rather than deferred, and it
**disproved the original assumption**. Numbers from `npm run build` plus a
sourcemap breakdown:

| | |
|---|---|
| initial chunk | **1,389.19 kB** (gzip 459.37 kB), single chunk, >500KB warning |
| echarts + zrender | **3,105 KB of source — 55% of the bundle** |
| removing the `LineChart` import | saved **0.68 kB**; echarts still present |

Other heavy Rnui dependencies (`recharts`, `shiki`, `date-fns`, `@dnd-kit`,
`cmdk`, `embla`) tree-shake away cleanly — they are absent from the built output.
Echarts does not, because `@e412/rnui-react` ships as a **single barrel module**
that imports `echarts/core`, `echarts/charts`, `echarts/renderers` and performs
echarts' side-effectful `use([...])` registration at module scope. Rollup cannot
prove that is safe to drop, and the package declares no `"sideEffects": false`.

**Consequence: changing our imports cannot remove echarts.** The chart swap and
the bundle fix are therefore two separate pieces of work, not one.

### Charts

`TimeSeriesChart` wraps **uPlot** (~40KB, purpose-built for dense time-series) and
is themed from the design tokens: `border` for axes and grid, `chart-*`/status
colours for series, IBM Plex Mono for tick labels. `MetricChartCard` composes it,
and `Sparkline` uses the same rendering path. This is the one place we step
outside Rnui — a deliberate trade for one coherent chart surface we control.

Justified on **fit and consistency**, not bundle size. Any bundle saving is a
consequence of the separate work below.

### Bundle

Ordered by preference; take the first that measurably works:

1. **Stub echarts at the bundler.** Once no Rnui chart component is used, alias
   `echarts/*` to an empty module in `vite.config.ts` so Rollup drops it. Contained
   entirely in this repo and worth ~55% of the bundle. Must be verified by actually
   exercising every screen — if any Rnui component we still use touches echarts at
   runtime, this breaks loudly and we fall back to (2).
2. **`manualChunks`.** Split echarts into its own chunk. Clears the 500KB warning
   and improves caching, but does not reduce bytes shipped.

The clean long-term fix is upstream, in `@e412/rnui-react` (published under your
own scope): declare `"sideEffects": false` and move echarts' `use()` registration
inside the chart component module so it tree-shakes per-component. Out of scope
here, but worth doing in that repo — it would benefit every consumer.

**Target:** initial chunk under Vite's 500KB warning threshold, with before/after
numbers reported.

`Sparkline` (the fleet grid's inline trend) is **also rebuilt on uPlot**, so every
chart surface in the app shares one rendering path, one theming source, and one
set of behaviours — no hand-maintained SVG path maths living alongside a real
charting library. It is a stripped uPlot configuration: no axes, no grid, no
legend, no cursor, no interaction.

**Density check.** The fleet grid renders two sparklines per row, so a 40-guest
fleet mounts ~80 chart instances. Measure this at realistic fleet size once the
component exists: if instance overhead visibly regresses the fleet page, the
fallback is to render sparklines from the same uPlot theme values via a static
path rather than a live instance. Decide by measurement, not assumption.

**Target:** initial chunk under Vite's 500KB warning threshold, with the
before/after numbers reported.

## Accessibility floor

Non-negotiable, and cheap if designed in rather than retrofitted:

- **Status is never encoded by colour alone.** Every machine row carries the
  status word in a `STATUS` column; every container row carries its state as text.
  The tag fill, the row rail, and the fleet strip are *reinforcement*. Fleet-strip
  blocks — which are colour-only by nature — are therefore never the sole
  representation of a machine's health, and each carries an accessible name
  (`aria-label` / `title`) of the form `hostname — status`.
- **Tag text contrast.** Tag labels default to black on the fill; where black does
  not reach 4.5:1 against a given fill (`fail #FF1744` is the borderline one),
  that tag uses white text instead. The rule is per-fill and verified, not assumed.
- **Body contrast.** `dim` on `ground` clears 4.5:1 in both themes
  (`#8A8A8A` on black ≈ 5.3:1; `#6B6B70` on white ≈ 5.5:1).
- **Visible keyboard focus** on every interactive element, using a `brand` outline
  with offset — never `outline: none`. Focus must remain visible against a black
  ground.
- **Reduced motion** respected (`prefers-reduced-motion`) for any transition added.
- Tables remain usable at a narrow viewport (horizontal scroll within the panel
  rather than the page).

## Verification

There is no frontend test harness, and this change is visual, so the gates are:

- `tsc --noEmit` (the real type gate — `npm run build` is `vite build` only and
  does **not** type-check).
- `npm run build` succeeds, and the initial chunk is under 500KB (report before
  and after). Confirm echarts is absent from the built output.
- Charts render correctly in both themes after the uPlot swap: axes, grid, tick
  labels, multi-series (the network rx/tx card), and the empty-data state.
- A visual pass served over LAN, checked in **both themes** and at a narrow
  viewport, covering: fleet (populated + empty), machine detail (populated,
  loading, not-found, error), and container rows in each state.
- Keyboard-only pass: tab through fleet → machine detail → a container verb, with
  focus visible at every stop.

## Out of scope

- Server, proto, agent changes.
- Units / Logs / Terminal tabs (their own slices).
- A frontend test harness.
- Deferred UX items from the Docker slice review: friendlier verb-error copy, and
  a health-badge colour treatment. Both are re-decided naturally by this system's
  status tokens; anything left over is picked up with the systemd slice.

---

## Amendments after live review

The design above was implemented, then reviewed running against a real fleet.
These changes came out of that review and **supersede** the corresponding
sections above. Recorded with their reasoning, because the reasoning is the part
that is expensive to reconstruct.

### The fleet strip is gone

The signature element was to be the asset tag *at two scales* — a row tag, plus a
strip of one block per machine at the top of the listing. **The strip is removed.**

It was designed for a specific job: answering "is anything red?" across ~40 guests
without reading a row. At the fleet sizes actually in use it sits directly above a
table that says the same thing, so it reads as decoration. The row tag alone now
carries the signature. If the fleet grows to the scale the strip was drawn for,
`components/FleetStrip.tsx` is recoverable from git history (deleted in
`a7deeb5`).

### Shell: a sidebar, not a top bar

Nav moved from a slim top bar into a **left sidebar** (`w-44`), with the fleet
summary and theme toggle in its footer. The driver was extensibility — audit,
tokens, and later per-machine logs/terminal need somewhere to go, and a sidebar
holds a growing list better than a horizontal bar.

The sidebar renders from a shared route config (`app/routes.tsx`) that
`App.tsx` also renders `<Routes>` from, so a new page becomes a route *and* a nav
entry in one place. Only routes that exist are listed — no disabled placeholders
for unbuilt sections, consistent with the tabs decision.

### Content is constrained, not full-bleed

The original "full-width, ops consoles go wide" call was wrong in practice: at
monitor width the tables stretched into unreadable ribbons. Content is now capped
at `max-w-6xl` (1152px) and centred. 1152px rather than something tighter because
the machines table has ten columns and would otherwise scroll horizontally.

### Light mode: an ink brand block, not a yellow one

`#FFE600` is right on black and glaring on white. In **light mode only**, brand
surfaces (the sidebar block, the active tab) invert: `--primary: #14161A` with
`--primary-foreground: #FFE600` — an ink block with yellow text (≈14.5:1). Dark
mode is unchanged. Status fills remain identical across themes, so the asset tag
itself is untouched.

### Machine page: no header card

The `PageHeader` card (large hostname tag, meta, status badge) is replaced by a
single slim line above the tabs: the hostname tag followed by a mono meta run.
The separate status chip is gone, but **the status word remains as the first item
of that meta run** — the tag's fill encodes status by colour, and the
accessibility floor requires status never be conveyed by colour alone, so a text
carrier has to survive somewhere on the page.

### Tables are bare, not carded

Both the machines listing and the containers panel dropped their rnui `Card`
wrapper for a plain `border-2 border-border` container. Card chrome around a table
that already has its own borders was a box inside a box. Section titles and counts
sit in a heading row above the border instead of in a card header.

### Routes are honest paths

The listing moved from `/` to **`/machines`** (`/machines/:id` for detail); `/`
now redirects. Every section gets a real URL rather than the listing squatting on
the root.

### Charts got tooltips

Moving from rnui's echarts `LineChart` to uPlot silently dropped the hover
readout: uPlot surfaces values through its legend, which was disabled for
single-series charts. A tooltip plugin now gives every chart a floating readout
(timestamp plus each series' value), and each card passes a unit-aware formatter
(`4.2%`, `0.42`, `1.2 MB/s`). The legend is kept non-live for multi-series charts
purely to identify rx/tx.

### The bundle target was not met, and the target moved

The spec targeted an initial chunk under Vite's 500 kB warning. Removing echarts
took it **1,389 → ~679 kB (−51%)**, and what remains is react-dom plus
rnui/base-ui — real application code, not bloat. Rather than leave a build warning
that fires forever, `build.chunkSizeWarningLimit` is raised to **750 kB**: above
the measured floor, low enough that genuine new bloat still trips it. Splitting
vendor chunks would clear the warning without shipping fewer bytes, and buys
little for an SPA embedded in the control-plane binary and served over the LAN.

### Second amendment: library components, and the machine header

A later review of the running app found three pieces of chrome hand-rolled when
the component library already provided them — the same objection three times over.
All three were replaced, and the pattern is now a standing rule (recorded in
`docs/DEV.md`): **check what `@e412/rnui-react` exports before building chrome.**

- **Sidebar** — the hand-rolled `<aside>` became rnui's `Sidebar` suite, which
  brings collapsing, a mobile sheet and a keyboard shortcut. rnui's own
  `--sidebar*` tokens are overridden in both themes so it renders in the Argus
  palette rather than the library's defaults. Width is rnui's 16rem default.
- **Nav active state** — a hand-rolled `isNavActive()` + `alsoActiveOn` scheme
  became React Router's `NavLink`.
- **Asset tag** — the bespoke `assetTagVariants` cva table became rnui's `Badge`.
  Note that Badge's `success`/`warning`/`destructive` variants hardcode
  `text-white`, so `AssetTag` forces `text-black` on every tone to hold the
  contrast rule (white on `fail #FF1744` is 3.85:1 and fails AA; black is 5.46:1).
- **Breadcrumb** — a bare `<div>` with a literal `" / "` became rnui's
  `Breadcrumb` suite, which supplies the nav landmark, list semantics and
  `aria-current`.

**Machine page header (supersedes "Machine page: no header card" above).** The
hostname tag plus run-on meta line is replaced by a breadcrumb, the hostname as an
`<h1>` title, and a **`SpecStrip`** — the facts as labelled cells, each value
under its own key. The run-on line forced parsing by position and got worse with
every field added; the strip scales instead. Optional fields render only when
present, so a machine missing a kernel simply has one fewer cell. The `Status`
cell is unconditional and rendered via `StatusBadge`, because with the hostname
now a plain title it is the page's only text carrier for status — the
accessibility floor requires status never be conveyed by colour alone.

**A cascade trap worth knowing**, documented in full in `docs/DEV.md`: when
overriding an rnui class, write the override with the *same Tailwind modifier the
base uses*. Two bugs in this branch came from not doing so — `data-[active=true]:`
against rnui's presence-form `data-active:`, and a plain `border-r-2` against
rnui's `group-data-[side=left]:border-r`. Both compiled, passed every gate, and
silently did nothing.
