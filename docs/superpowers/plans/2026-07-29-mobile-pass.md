# Mobile / Responsive Pass Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fleet renders as tap-through cards on phones, unit/container verbs are reachable at 390px without sideways scroll, and Argus installs to a home screen with a proper icon (manifest, no service worker).

**Architecture:** Pure frontend plus one two-line server touch (a `webmanifest` MIME arm). The fleet gains a second renderer (cards below `md`) over the same `lib/fleet.ts` outputs; tables hide low-value columns below `md`; static PWA assets land in `frontend/public/` which Vite copies into `dist/` and rust-embed serves.

**Tech Stack:** Tailwind responsive variants (`md:`), rnui components already in use, a one-off headless-chromium icon render (scratchpad playwright), no new dependencies.

**Design of record:** `docs/superpowers/specs/2026-07-29-mobile-pass-design.md`.

## Global Constraints

- **Breakpoint:** `md` is THE mobile/desktop boundary for this slice — cards vs table, hidden vs shown columns. No per-component breakpoint improvisation.
- **Terminal gets zero mobile work** (stays reachable). No service worker, no offline, no push.
- **No new dependencies** (dev or runtime). Icons are committed PNGs, generated once.
- **Identity colors:** background `#000000`, hazard yellow `#FFD60A` — read the REAL value from `frontend/src/index.css`'s token overrides before generating icons; if it differs, the CSS is the truth, not this plan.
- **Gates per task:** `npm --prefix frontend run typecheck && npm --prefix frontend run build`; for the server touch additionally `cargo fmt --all --check`, `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p argus-server`. Never `cargo test --workspace -- --ignored`.
- Visual verification is deferred to the controller's phone-width browser pass — implementers note it, never attempt screenshots.

---

### Task 1: PWA installability — manifest, icons, index.html, server MIME

**Files:**
- Create: `frontend/public/manifest.webmanifest`, `frontend/public/icon-192.png`, `frontend/public/icon-512.png`, `frontend/public/icon-maskable-512.png`, `frontend/public/apple-touch-icon.png` (180×180)
- Modify: `frontend/index.html`, `crates/server/src/http.rs` (`content_type` + one test line)

**Interfaces:**
- Produces: `/manifest.webmanifest` and the icon set served by both vite dev and the embedded build with correct MIME.

- [ ] **Step 1: Generate the icons.** Write this script to the session scratchpad and run it with the scratchpad's playwright (`PLAYWRIGHT_BROWSERS_PATH=<scratchpad>/pw-browsers`; if absent, `npm i playwright && npx playwright install chromium --with-deps` inside the scratchpad — never into the repo):

```js
// icon-gen.mjs — renders the Argus launcher icons: hazard-yellow Archivo
// Black "A" on pure black, square corners (the asset-tag identity).
import { chromium } from "playwright";
import { readFileSync } from "node:fs";

const fontPath =
  "/home/aloks98/projects/argus/frontend/node_modules/@fontsource/archivo-black/files/archivo-black-latin-400-normal.woff2";
const fontB64 = readFileSync(fontPath).toString("base64");

// PLAIN icon: glyph sized generously. MASKABLE: same glyph inside the 80%
// safe zone (Android crops up to a circle — the glyph must survive it).
function page(size, scale) {
  return `<!doctype html><style>
  @font-face { font-family: AB; src: url(data:font/woff2;base64,${fontB64}) format("woff2"); }
  html,body { margin:0; width:${size}px; height:${size}px; background:#000; }
  div { width:100%; height:100%; display:flex; align-items:center; justify-content:center;
        font-family:AB; color:#FFD60A; font-size:${Math.round(size * scale)}px; }
  </style><div>A</div>`;
}

const browser = await chromium.launch();
const jobs = [
  ["icon-192.png", 192, 0.62],
  ["icon-512.png", 512, 0.62],
  ["icon-maskable-512.png", 512, 0.44],
  ["apple-touch-icon.png", 180, 0.62],
];
for (const [name, size, scale] of jobs) {
  const p = await browser.newPage({ viewport: { width: size, height: size } });
  await p.setContent(page(size, scale));
  await p.evaluate(() => document.fonts.ready);
  await p.screenshot({ path: `/home/aloks98/projects/argus/frontend/public/${name}` });
  await p.close();
}
await browser.close();
console.log("icons written");
```

Create `frontend/public/` first. Before running, confirm the yellow: grep `frontend/src/index.css` for the primary/hazard token override and use ITS hex in the script if it differs from `#FFD60A`. After running, sanity-check each PNG's pixel size (`file frontend/public/*.png`).

- [ ] **Step 2: Write the manifest**

```json
{
  "name": "Argus",
  "short_name": "Argus",
  "description": "Fleet management for the homelab",
  "start_url": "/",
  "display": "standalone",
  "background_color": "#000000",
  "theme_color": "#000000",
  "icons": [
    { "src": "/icon-192.png", "sizes": "192x192", "type": "image/png" },
    { "src": "/icon-512.png", "sizes": "512x512", "type": "image/png" },
    { "src": "/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable" }
  ]
}
```

- [ ] **Step 3: index.html head additions** (after the `<title>` line; there is currently no favicon at all):

```html
    <link rel="icon" type="image/png" href="/icon-192.png" />
    <link rel="apple-touch-icon" href="/apple-touch-icon.png" />
    <link rel="manifest" href="/manifest.webmanifest" />
    <meta name="theme-color" content="#000000" />
```

- [ ] **Step 4: server MIME arm.** In `crates/server/src/http.rs::content_type`, add before the catch-all:

```rust
        Some("webmanifest") => "application/manifest+json",
```

Extend the existing static-handler test coverage with one assertion that a `.webmanifest` path is served with that content type (follow how the current tests exercise `static_handler`/`content_type`; if only `content_type` is unit-testable, a direct `assert_eq!(content_type("manifest.webmanifest"), "application/manifest+json")` in the existing test module is enough).

- [ ] **Step 5: Gates.** `npm --prefix frontend run build`, then verify `dist/manifest.webmanifest` and all four PNGs exist in `dist/` (vite copies `public/` verbatim). Run the server gates (fmt, clippy offline, `cargo test -p argus-server`).

- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat(mobile): PWA manifest, launcher icons, and webmanifest MIME"`

---

### Task 2: Fleet card list below `md`

**Files:**
- Modify: `frontend/src/pages/FleetPage.tsx`

**Interfaces:**
- Consumes: `visibleFleet`/`groupFleet`/`displayName` (unchanged), the existing `FleetTable`, `StatusCell`, `Sparkline`, `formatPct`, `formatRelative`, `machineTone`.

- [ ] **Step 1: Extract the status cluster.** `StatusCell` is already a component; ensure it is reusable by the card (no table-specific markup inside it — if there is, split the badge cluster out).

- [ ] **Step 2: Add the card renderer** in the same file:

```tsx
/** Phone rendering of one fleet row: the whole card is a single tap target
 *  (design "Fleet: card list below md"). Same data, same helpers as the
 *  table — this is a second renderer, not a second data path. */
function FleetCards({ rows }: { rows: FleetRow[] }) {
  return (
    <ul className="flex flex-col divide-y divide-border">
      {rows.map((row) => (
        <li key={row.id}>
          <Link to={`/machines/${row.id}`} className="flex flex-col gap-2 p-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <AssetTag tone={machineTone(row.status)}>{displayName(row)}</AssetTag>
              <StatusCell row={row} />
            </div>
            {row.display_name !== null && (
              <span className="font-mono text-[11px] text-muted-foreground">{row.hostname}</span>
            )}
            <div className="flex items-center gap-4">
              <span className="flex items-center gap-2 font-mono text-xs">
                CPU {formatPct(row.cpu_pct)} <Sparkline values={row.spark_cpu} />
              </span>
              <span className="flex items-center gap-2 font-mono text-xs">
                Mem {formatPct(row.mem_pct)} <Sparkline values={row.spark_mem} />
              </span>
            </div>
            <span className="font-mono text-[11px] text-muted-foreground">
              seen {formatRelative(row.last_seen_at)}
            </span>
          </Link>
        </li>
      ))}
    </ul>
  );
}
```

(Exact classes are a starting point; match the app's existing spacing idiom. If `AssetTag`/`StatusCell` imports or props differ, the file is the truth.)

- [ ] **Step 3: Swap by breakpoint.** Everywhere `FleetTable` renders (flat view AND each grouped section), render both with visibility classes:

```tsx
<div className="hidden md:block"><FleetTable rows={...} /></div>
<div className="md:hidden"><FleetCards rows={...} /></div>
```

Keep the shared bordered wrapper/empty states working in both modes (empty states are breakpoint-independent — do not duplicate them).

- [ ] **Step 4: Gates** (typecheck + build). Note in the report that visual verification is the controller's.

- [ ] **Step 5: Commit** — `git commit -am "feat(mobile): fleet card list below md"`

---

### Task 3: Verbs reachable at 390px — Units/Containers column hiding

**Files:**
- Modify: `frontend/src/components/UnitsCard.tsx`, `frontend/src/components/ContainersCard.tsx`

- [ ] **Step 1: UnitsCard.** Add `className="hidden md:table-cell"` to the **Sub** and **Description** `TableHead`s and their corresponding `TableCell`s (the Description cell already has classes — merge, don't replace). Tighten the Name cap for small screens: the AssetTag's `max-w-[30ch]` becomes `max-w-[16ch] md:max-w-[30ch]`. Do not touch the Actions cell's fixed-position-per-row invariant (documented in the cell's comment — read it first).

- [ ] **Step 2: ContainersCard.** Same treatment on **Image** and **Status** columns (State stays — it carries the tone badge; the textual Status duplicates it on a phone). Check whether the Name cell has a width cap; if unbounded, cap it like UnitsCard's at small widths.

- [ ] **Step 3: Consistency check.** Both files: confirm every hidden `TableHead` has its `TableCell` hidden with the SAME variant chain, or columns shear (header count ≠ cell count per row is a silent layout break, not an error).

- [ ] **Step 4: Gates** (typecheck + build).

- [ ] **Step 5: Commit** — `git commit -am "feat(mobile): unit/container verbs reachable at phone width"`

---

## Final verification (controller, not a task)

- Whole-branch review (small slice — mid-tier reviewer is proportionate).
- Headless phone-width pass, both themes: fleet cards (flat + grouped + filtered-empty), units/containers verb reachability at 390px, the three dialogs, sign-in, logs viewer.
- Installability: `/manifest.webmanifest` 200 + correct MIME from the EMBEDDED build (rebuild server, curl), icons 200, manifest parses.
- Live: user installs to a real phone (iOS-style add works over LAN HTTP; Chrome's prompt waits for the production HTTPS origin per the spec's caveat) and taps fleet → machine → restart a unit.
- DEV.md gains the E2E record.
