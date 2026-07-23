# Frontend Design System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Argus SPA a deliberate visual identity (asset-tag direction, hazard yellow on black) and a shared component/data layer, removing the duplication that would otherwise be copied into the systemd, logs, and terminal slices.

**Architecture:** Design tokens install as CSS-variable overrides on Rnui's existing shadcn-style variables, so every Rnui component inherits the identity. TanStack Query replaces all hand-rolled polling and loading/error state. Component variants become typed `cva` tables. Charts move from Rnui's echarts-backed `LineChart` to a uPlot component we own.

**Tech Stack:** React 19, TypeScript, Vite 7, Tailwind v4, `@e412/rnui-react`, TanStack Query v5, uPlot, cva.

Design of record: `docs/superpowers/specs/2026-07-23-frontend-design-system-design.md`.

## Global Constraints

- **Palette (dark, default):** `ground #000000` · `ink #F5F5F5` · `dim #8A8A8A` · `rule #242424` · `brand #FFE600` · `ok #00E676` · `warn #FF6D00` · `fail #FF1744`.
- **Palette (light):** `ground #FFFFFF` · `ink #0A0A0B` · `dim #6B6B70` · `rule #D4D4D8`. Brand and status **fills** are unchanged across themes.
- **Light-mode text variants** (bright fills fail as text on white): brand `#8A7300`, ok `#007A3D`, warn `#C24E00`, fail `#C4001D`.
- **`--radius: 0`** everywhere. Boldness comes from solid fills and 2px rules, never hairlines.
- **Type:** Archivo Black (display, sparing) · Archivo (UI) · IBM Plex Mono (all machine truth: hostnames, IPs, container ids, `Exited (2)`, uptime, byte counts). Self-hosted via `@fontsource` — **no CDN**, the SPA is embedded in the Rust binary.
- **Status is never encoded by colour alone.** Every machine row carries a `STATUS` text column; every container row carries its state as text. Fleet-strip blocks carry `aria-label` of the form `hostname — status`.
- **Tag text contrast:** black on the fill by default; `fail #FF1744` uses white text if black does not reach 4.5:1.
- **Visible keyboard focus** on every interactive element (brand outline with offset). Never `outline: none`.
- **No new runtime deps beyond those listed.** No date library (`Intl.RelativeTimeFormat` is built in), no TanStack Table, no zod.
- **Exact versions:** `@tanstack/react-query@5.101.4`, `uplot@1.6.32`, `uplot-react@1.2.4`, `class-variance-authority@0.7.1`, `clsx@2.1.1`, `tailwind-merge@3.6.0`, `@fontsource/archivo@5.3.0`, `@fontsource/archivo-black@5.3.0`, `@fontsource/ibm-plex-mono@5.3.0`.

### Verification model (read this before Task 1)

**There is no frontend test harness, and adding one is out of scope.** So the per-task cycle is *not* TDD. Every task ends with:

```bash
npx tsc --noEmit          # the REAL type gate
npm run build             # `npm run build` is `vite build` only — it does NOT type-check
```

Both must be clean. Plus the visual check named in that task, served over LAN:

```bash
cd frontend && npm run dev -- --host      # http://<box-ip>:5173
```

**Measured baseline to beat (2026-07-23):** initial chunk **1,389.19 kB** (gzip 459.37 kB), CSS 172.14 kB, one chunk, Vite >500KB warning.

---

### Task 1: Design tokens, fonts, sharp corners

Installs the identity. After this task the whole app changes colour and type without any structural change — the fastest way to see the direction is real.

**Files:**
- Modify: `frontend/package.json` (font deps)
- Modify: `frontend/src/index.css`

**Interfaces:**
- Produces: CSS custom properties consumed by every later task — `--background`, `--foreground`, `--muted-foreground`, `--border`, `--primary`, `--success`, `--warning`, `--destructive`, `--chart-1..5`, `--radius`, plus the `--*-text` light-mode variants. Tailwind font utilities `font-sans`, `font-mono`, `font-display`.

- [ ] **Step 1: Install the self-hosted faces**

```bash
cd frontend
npm install --save-exact @fontsource/archivo@5.3.0 @fontsource/archivo-black@5.3.0 @fontsource/ibm-plex-mono@5.3.0
```

- [ ] **Step 2: Replace `frontend/src/index.css` entirely**

The existing three lines must be kept and extended — Tailwind v4 does not scan `node_modules`, so dropping `@source` silently breaks every Rnui utility class.

```css
@import "tailwindcss";
/* rnui design tokens + base styles + the @theme color mapping (light/dark). */
@import "@e412/rnui-themes";
/* Tailwind v4 does not scan node_modules by default; point it at the rnui
   library so the utility classes its compiled components use are generated. */
@source "../node_modules/@e412/rnui-react/dist";

/* Self-hosted faces (no CDN — this SPA is embedded in the Rust binary). */
@import "@fontsource/archivo/400.css";
@import "@fontsource/archivo/600.css";
@import "@fontsource/archivo/700.css";
@import "@fontsource/archivo-black/400.css";
@import "@fontsource/ibm-plex-mono/400.css";
@import "@fontsource/ibm-plex-mono/600.css";

@theme {
  --font-sans: "Archivo", system-ui, sans-serif;
  --font-mono: "IBM Plex Mono", ui-monospace, monospace;
  --font-display: "Archivo Black", "Archivo", system-ui, sans-serif;
}

/* ─── Argus identity: asset tag, hazard yellow on black ───────────────────
   These override the rnui-themes values. Brand and status FILLS are the same
   in both themes (that is what makes the asset tag ground-agnostic); only the
   chrome and the *-text variants change. */

:root {
  --radius: 0rem;

  --background: #FFFFFF;
  --foreground: #0A0A0B;
  --card: #FFFFFF;
  --card-foreground: #0A0A0B;
  --popover: #FFFFFF;
  --popover-foreground: #0A0A0B;
  --muted: #F1F1F3;
  --muted-foreground: #6B6B70;
  --secondary: #F1F1F3;
  --secondary-foreground: #0A0A0B;
  --accent: #F1F1F3;
  --accent-foreground: #0A0A0B;
  --border: #D4D4D8;
  --input: #D4D4D8;

  --primary: #FFE600;
  --primary-foreground: #000000;
  --success: #00E676;
  --success-foreground: #000000;
  --warning: #FF6D00;
  --warning-foreground: #000000;
  --destructive: #FF1744;
  --destructive-foreground: #FFFFFF;

  --ring: #8A7300;
  --focus: #8A7300;
  --focus-foreground: #FFFFFF;

  --chart-1: #FFE600;
  --chart-2: #00E676;
  --chart-3: #FF6D00;
  --chart-4: #FF1744;
  --chart-5: #6B6B70;

  /* Bright fills are unreadable as TEXT on white — darkened variants. */
  --brand-text: #8A7300;
  --ok-text: #007A3D;
  --warn-text: #C24E00;
  --fail-text: #C4001D;
}

.dark {
  --background: #000000;
  --foreground: #F5F5F5;
  --card: #000000;
  --card-foreground: #F5F5F5;
  --popover: #000000;
  --popover-foreground: #F5F5F5;
  --muted: #121212;
  --muted-foreground: #8A8A8A;
  --secondary: #121212;
  --secondary-foreground: #F5F5F5;
  --accent: #121212;
  --accent-foreground: #F5F5F5;
  --border: #242424;
  --input: #242424;

  --primary: #FFE600;
  --primary-foreground: #000000;
  --success: #00E676;
  --success-foreground: #000000;
  --warning: #FF6D00;
  --warning-foreground: #000000;
  --destructive: #FF1744;
  --destructive-foreground: #FFFFFF;

  --ring: #FFE600;
  --focus: #FFE600;
  --focus-foreground: #000000;

  --chart-1: #FFE600;
  --chart-2: #00E676;
  --chart-3: #FF6D00;
  --chart-4: #FF1744;
  --chart-5: #8A8A8A;

  /* On black the fills are already legible as text. */
  --brand-text: #FFE600;
  --ok-text: #00E676;
  --warn-text: #FF6D00;
  --fail-text: #FF1744;
}

/* Focus must stay visible against a black ground. */
:where(a, button, [role="tab"], [tabindex]):focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 2px;
}
```

- [ ] **Step 3: Verify**

```bash
cd frontend && npx tsc --noEmit && npm run build
```
Both clean. Then `npm run dev -- --host` and confirm on the fleet page: background is pure black, text is Archivo, the theme toggle still flips to a white ground, and corners are square. Existing hard-coded `text-gray-600` will still look wrong — Task 4 removes those.

- [ ] **Step 4: Commit**

```bash
git add frontend/package.json frontend/package-lock.json frontend/src/index.css
git commit -m "feat(frontend): install Argus design tokens, self-hosted type, square corners"
```

---

### Task 2: `cn` helper and typed status variants

Replaces the three hand-rolled `Record<string, …>` variant maps with one typed `cva` table, and introduces the signature `AssetTag`.

**Files:**
- Modify: `frontend/package.json`
- Create: `frontend/src/lib/cn.ts`, `frontend/src/lib/status.ts`
- Create: `frontend/src/components/AssetTag.tsx`, `frontend/src/components/StatusBadge.tsx`

**Interfaces:**
- Consumes: tokens from Task 1.
- Produces:
  - `cn(...inputs: ClassValue[]): string`
  - `type Tone = "ok" | "warn" | "fail" | "idle"`
  - `machineTone(status: string): Tone`, `containerTone(state: string): Tone`
  - `<AssetTag tone={Tone}>{children}</AssetTag>`
  - `<StatusBadge tone={Tone} label={string} />`

- [ ] **Step 1: Install**

```bash
cd frontend
npm install --save-exact class-variance-authority@0.7.1 clsx@2.1.1 tailwind-merge@3.6.0
```

- [ ] **Step 2: Create `frontend/src/lib/cn.ts`**

```ts
import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** Merge conditional class names, with later Tailwind utilities winning. */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
```

- [ ] **Step 3: Create `frontend/src/lib/status.ts`**

One tone vocabulary for both machine status and container state, so a single
variant table drives every status surface in the app.

```ts
import { cva, type VariantProps } from "class-variance-authority";

/** The semantic tones. Colour never carries status alone — always paired with text. */
export type Tone = "ok" | "warn" | "fail" | "idle";

/** machines.status from the control plane. */
export function machineTone(status: string): Tone {
  switch (status) {
    case "online":
      return "ok";
    case "pending":
      return "warn";
    case "offline":
      return "fail";
    default:
      return "idle";
  }
}

/** Docker container state (running|exited|paused|restarting|created|dead|removing). */
export function containerTone(state: string): Tone {
  switch (state) {
    case "running":
      return "ok";
    case "restarting":
    case "paused":
      return "warn";
    case "dead":
      return "fail";
    case "exited":
    case "created":
    case "removing":
      return "idle";
    default:
      return "idle";
  }
}

/**
 * The signature element: a solid stencilled label whose fill is the status.
 * `fail` uses white text — black does not reach 4.5:1 on #FF1744.
 */
export const assetTagVariants = cva(
  "inline-block px-2 py-0.5 font-mono text-xs font-bold uppercase tracking-wider",
  {
    variants: {
      tone: {
        ok: "bg-[var(--success)] text-black",
        warn: "bg-[var(--warning)] text-black",
        fail: "bg-[var(--destructive)] text-white",
        idle: "bg-[var(--muted-foreground)] text-black",
      },
    },
    defaultVariants: { tone: "idle" },
  },
);
export type AssetTagVariants = VariantProps<typeof assetTagVariants>;

/** Text-only status, using the theme-aware *-text variants (readable on white). */
export const statusTextVariants = cva("font-mono text-xs uppercase tracking-wider", {
  variants: {
    tone: {
      ok: "text-[var(--ok-text)]",
      warn: "text-[var(--warn-text)]",
      fail: "text-[var(--fail-text)]",
      idle: "text-[var(--muted-foreground)]",
    },
  },
  defaultVariants: { tone: "idle" },
});
```

- [ ] **Step 4: Create `frontend/src/components/AssetTag.tsx`**

```tsx
import { assetTagVariants } from "../lib/status";
import type { Tone } from "../lib/status";
import { cn } from "../lib/cn";

/**
 * A hostname (or container name) rendered as a solid stencilled asset tag whose
 * fill is its status — the design's signature element.
 */
export default function AssetTag({
  tone,
  children,
  className,
}: {
  tone: Tone;
  children: React.ReactNode;
  className?: string;
}) {
  return <span className={cn(assetTagVariants({ tone }), className)}>{children}</span>;
}
```

- [ ] **Step 5: Create `frontend/src/components/StatusBadge.tsx`**

```tsx
import { statusTextVariants } from "../lib/status";
import type { Tone } from "../lib/status";
import { cn } from "../lib/cn";

/**
 * The text half of a status. Always rendered alongside any colour cue so status
 * is never conveyed by colour alone.
 */
export default function StatusBadge({
  tone,
  label,
  className,
}: {
  tone: Tone;
  label: string;
  className?: string;
}) {
  return <span className={cn(statusTextVariants({ tone }), className)}>{label}</span>;
}
```

- [ ] **Step 6: Verify and commit**

```bash
cd frontend && npx tsc --noEmit && npm run build
```
Both clean. (Nothing renders these yet — Task 4 wires them in.)

```bash
git add frontend/package.json frontend/package-lock.json frontend/src/lib frontend/src/components
git commit -m "feat(frontend): cn helper and typed cva status variants with AssetTag"
```

---

### Task 3: TanStack Query data layer

Deletes every hand-rolled poll effect, loading/error triple, the manual `refetchDocker()`, and the `busy` Set.

**Files:**
- Modify: `frontend/package.json`, `frontend/src/main.tsx`
- Create: `frontend/src/lib/queries.ts`
- Modify: `frontend/src/FleetPage.tsx`, `frontend/src/MachineDetailPage.tsx`

**Interfaces:**
- Consumes: existing `frontend/src/api.ts` functions unchanged (`getFleet`, `getMachine`, `getMetrics`, `getDocker`, `containerAction`).
- Produces: `qk` (query keys), `useFleet()`, `useMachine(id)`, `useMetrics(id, range)`, `useDocker(id)`, `useContainerAction(id)`.

- [ ] **Step 1: Install**

```bash
cd frontend
npm install --save-exact @tanstack/react-query@5.101.4
```

- [ ] **Step 2: Wrap the app in `frontend/src/main.tsx`**

Add the import, create one client at module scope, and nest the provider **inside** `ThemeProvider` so the existing theme behaviour is untouched:

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // Polling is per-query via refetchInterval; keep data on screen while
      // refetching so a poll never flashes a loading state over live data.
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});
```

Wrap the existing `<App />` (keep `ThemeProvider` and its props exactly as they are):

```tsx
<QueryClientProvider client={queryClient}>
  <App />
</QueryClientProvider>
```

- [ ] **Step 3: Create `frontend/src/lib/queries.ts`**

```ts
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  containerAction,
  getDocker,
  getFleet,
  getMachine,
  getMetrics,
} from "../api";
import type { ContainerAction } from "../api";

/** Polling cadences (ms). Fleet is the scan view, so it refreshes faster. */
const FLEET_INTERVAL = 5_000;
const MACHINE_INTERVAL = 10_000;

export type Range = "1h" | "6h" | "24h";

/** Every query key in the app lives here so no screen invents its own. */
export const qk = {
  fleet: ["fleet"] as const,
  machine: (id: string) => ["machine", id] as const,
  metrics: (id: string, range: Range) => ["metrics", id, range] as const,
  docker: (id: string) => ["docker", id] as const,
};

export function useFleet() {
  return useQuery({
    queryKey: qk.fleet,
    queryFn: getFleet,
    refetchInterval: FLEET_INTERVAL,
  });
}

export function useMachine(id: string) {
  return useQuery({
    queryKey: qk.machine(id),
    queryFn: () => getMachine(id),
    refetchInterval: MACHINE_INTERVAL,
  });
}

export function useMetrics(id: string, range: Range) {
  return useQuery({
    queryKey: qk.metrics(id, range),
    queryFn: () => getMetrics(id, range),
    refetchInterval: MACHINE_INTERVAL,
  });
}

export function useDocker(id: string) {
  return useQuery({
    queryKey: qk.docker(id),
    queryFn: () => getDocker(id),
    refetchInterval: MACHINE_INTERVAL,
  });
}

/**
 * Container verbs. On success the docker snapshot is invalidated so the panel
 * reflects the new state without waiting for the next poll — this replaces the
 * manual refetchDocker(). Per-row in-flight state comes from `variables`, which
 * replaces the hand-rolled busy Set.
 */
export function useContainerAction(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { container: string; action: ContainerAction }) =>
      containerAction(id, vars.container, vars.action),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: qk.docker(id) });
    },
  });
}
```

- [ ] **Step 4: Migrate `FleetPage.tsx` to the hook**

Delete the `useEffect`/`setInterval`/`cancelled` block and the `rows`/`error`/`loading` `useState`s, and replace the top of the component with:

```tsx
const { data: rows = [], error, isPending } = useFleet();
```

Update the two render sites: the error `Alert` renders when `error !== null` becomes `error != null` using `error.message`, and the `CardDescription` uses `isPending` where it used `loading`. Remove the now-unused `useEffect`/`useState` imports and the `POLL_INTERVAL_MS` constant.

- [ ] **Step 5: Migrate `MachineDetailPage.tsx` to the hooks**

Replace the `useEffect` poll block and the `machine`/`metrics`/`containers`/`loading`/`notFound`/`error` `useState`s with:

```tsx
const machineQuery = useMachine(id as string);
const metricsQuery = useMetrics(id as string, range);
const dockerQuery = useDocker(id as string);

const machine = machineQuery.data ?? null;
const metrics = metricsQuery.data ?? [];
const containers = dockerQuery.data ?? [];
const isPending = machineQuery.isPending;
const notFound = machineQuery.error?.message === "machine 404";
const error = machineQuery.error;
```

Delete `refetchDocker` entirely and pass nothing in its place — `ContainersCard` now owns its mutation (next step). Remove `POLL_INTERVAL_MS`.

- [ ] **Step 6: Replace `ContainersCard`'s busy Set with the mutation**

Inside `ContainersCard`, delete the `busy` state, the `run` function, and the `onChanged` prop. Replace with:

```tsx
const action = useContainerAction(machineId);
const actionError = action.error;
```

Per-row in-flight becomes:

```tsx
const rowBusy = action.isPending && action.variables?.container === c.id;
```

and each button's handler becomes:

```tsx
onClick={() => action.mutate({ container: c.id, action: "restart" })}
```

(likewise `"stop"` and `"start"`). Update the caller in `MachineDetailPage` to `<ContainersCard machineId={id} containers={containers} />`.

- [ ] **Step 7: Verify**

```bash
cd frontend && npx tsc --noEmit && npm run build
```
Both clean. Then `npm run dev -- --host` and confirm against a live control plane: the fleet list still refreshes, and **it no longer flashes "Loading…" on each poll**. Stop/start a container and confirm the row updates without a manual refresh. With the agent stopped, a verb shows the 409 error.

- [ ] **Step 8: Commit**

```bash
git add frontend/package.json frontend/package-lock.json frontend/src/main.tsx frontend/src/lib/queries.ts frontend/src/FleetPage.tsx frontend/src/MachineDetailPage.tsx
git commit -m "feat(frontend): move server state to TanStack Query, dropping hand-rolled polling"
```

---

### Task 4: Shell, structure, and the page split

The big restructure: directory layout, shared shell, extracted utils, and the end of the 526-line file. This is also where the hard-coded grays die.

**Files:**
- Create: `frontend/src/app/AppShell.tsx`
- Create: `frontend/src/components/PageHeader.tsx`, `frontend/src/components/Tabs.tsx`, `frontend/src/components/FleetStrip.tsx`, `frontend/src/components/ContainersCard.tsx`
- Create: `frontend/src/lib/format.ts`, `frontend/src/lib/metrics.ts`
- Move: `FleetPage.tsx`, `MachineDetailPage.tsx` → `frontend/src/pages/`; `Sparkline.tsx`, `ThemeToggle.tsx` → `frontend/src/components/`
- Modify: `frontend/src/App.tsx`, `frontend/src/api.ts` (imports only)

**Interfaces:**
- Consumes: `cn`, `AssetTag`, `StatusBadge`, `machineTone`, `containerTone` (Task 2); query hooks (Task 3).
- Produces:
  - `formatRelative(iso: string | null): string` — `"12s"` / `"3d"` / `"never"` via `Intl.RelativeTimeFormat`
  - `formatPct(v: number | null): string`, `formatBytesPerSec(v: number): string`
  - `buildCpuSeries`, `buildMemSeries`, `buildLoadSeries`, `buildNetRateSeries` (moved verbatim from `MachineDetailPage`), plus `type ChartPoint = { ts: string; value: number }` and `type NetRatePoint = { ts: string; rx: number; tx: number }`
  - `<AppShell>`, `<PageHeader>`, `<Tabs>`, `<FleetStrip>`

- [ ] **Step 1: Create `frontend/src/lib/format.ts`**

`formatLastSeen` currently exists byte-identically in both pages; this is its single home. Relative time uses the built-in `Intl.RelativeTimeFormat` — no date library.

```ts
const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto", style: "narrow" });

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["day", 86_400_000],
  ["hour", 3_600_000],
  ["minute", 60_000],
  ["second", 1_000],
];

/** "12s ago" style, compact. Returns "never" for a null timestamp. */
export function formatRelative(iso: string | null): string {
  if (iso === null) return "never";
  const delta = Date.parse(iso) - Date.now();
  for (const [unit, ms] of UNITS) {
    if (Math.abs(delta) >= ms || unit === "second") {
      return rtf.format(Math.round(delta / ms), unit);
    }
  }
  return "never";
}

/** Absolute timestamp, for tooltips where the exact time matters. */
export function formatAbsolute(iso: string | null): string {
  return iso === null ? "never" : new Date(iso).toLocaleString();
}

export function formatPct(pct: number | null): string {
  return pct == null ? "—" : `${pct.toFixed(0)}%`;
}

export function formatBytesPerSec(v: number): string {
  const units = ["B/s", "KB/s", "MB/s", "GB/s"];
  let n = v;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(1)} ${units[i]}`;
}
```

- [ ] **Step 2: Create `frontend/src/lib/metrics.ts`**

Move `ChartPoint`, `NetRatePoint`, `buildCpuSeries`, `buildMemSeries`, `buildLoadSeries`, and `buildNetRateSeries` out of `MachineDetailPage.tsx` **verbatim** (they are already pure and correct — including the counter-reset clamp and the `dtSeconds <= 0` guard). Export each, and `import type { MetricPoint } from "../api";` at the top.

- [ ] **Step 3: Create `frontend/src/app/AppShell.tsx`**

```tsx
import { Link } from "react-router-dom";
import ThemeToggle from "../components/ThemeToggle";

/**
 * The persistent chrome: brand block, primary nav, fleet summary, theme toggle.
 * Full-width — an ops console should not waste screen on a 10-column table.
 */
export default function AppShell({
  summary,
  children,
}: {
  summary?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="min-h-screen bg-background text-foreground font-sans">
      <header className="flex items-center border-b-2 border-border">
        <Link
          to="/"
          className="bg-primary px-3 py-2 font-display text-sm tracking-widest text-primary-foreground"
        >
          ARGUS
        </Link>
        <nav className="flex">
          <Link
            to="/"
            className="px-3 py-2 text-[10px] uppercase tracking-widest text-muted-foreground hover:text-foreground"
          >
            Fleet
          </Link>
        </nav>
        <div className="ml-auto flex items-center gap-3 px-3">
          {summary !== undefined && (
            <span className="font-mono text-[11px] text-muted-foreground">{summary}</span>
          )}
          <ThemeToggle />
        </div>
      </header>
      <main className="p-4">{children}</main>
    </div>
  );
}
```

- [ ] **Step 4: Create `frontend/src/components/PageHeader.tsx`**

```tsx
import { cn } from "../lib/cn";

/** Uppercase display title with an optional right-hand slot. */
export default function PageHeader({
  title,
  meta,
  right,
  className,
}: {
  title: React.ReactNode;
  meta?: React.ReactNode;
  right?: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex flex-wrap items-center justify-between gap-2 py-3", className)}>
      <div>
        <h1 className="font-display text-xl uppercase tracking-tight">{title}</h1>
        {meta !== undefined && (
          <p className="mt-1 font-mono text-xs text-muted-foreground">{meta}</p>
        )}
      </div>
      {right}
    </div>
  );
}
```

- [ ] **Step 5: Create `frontend/src/components/Tabs.tsx`**

Only tabs with content are passed in — Units/Logs/Terminal arrive with their slices.

```tsx
import { cn } from "../lib/cn";

export type TabKey = string;

export default function Tabs({
  tabs,
  active,
  onChange,
}: {
  tabs: { key: TabKey; label: string }[];
  active: TabKey;
  onChange: (key: TabKey) => void;
}) {
  return (
    <div role="tablist" className="flex border-y-2 border-border">
      {tabs.map((t) => (
        <button
          key={t.key}
          role="tab"
          aria-selected={t.key === active}
          onClick={() => onChange(t.key)}
          className={cn(
            "px-3 py-2 text-[10px] uppercase tracking-widest",
            t.key === active
              ? "bg-primary font-bold text-primary-foreground"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {t.label}
        </button>
      ))}
    </div>
  );
}
```

- [ ] **Step 6: Create `frontend/src/components/FleetStrip.tsx`**

The signature at fleet scale. Colour-only by nature, so every block carries an accessible name.

```tsx
import { Link } from "react-router-dom";
import { machineTone } from "../lib/status";
import { cn } from "../lib/cn";
import type { FleetRow } from "../api";

const TONE_BG: Record<string, string> = {
  ok: "bg-[var(--success)]",
  warn: "bg-[var(--warning)]",
  fail: "bg-[var(--destructive)]",
  idle: "bg-[var(--muted-foreground)]",
};

/** One block per machine — answers "is anything red?" without reading a row. */
export default function FleetStrip({ rows }: { rows: FleetRow[] }) {
  if (rows.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-1 py-2">
      {rows.map((r) => (
        <Link
          key={r.id}
          to={`/machines/${r.id}`}
          aria-label={`${r.hostname} — ${r.status}`}
          title={`${r.hostname} — ${r.status}`}
          className={cn("block h-4 w-6", TONE_BG[machineTone(r.status)])}
        />
      ))}
    </div>
  );
}
```

- [ ] **Step 7: Move files and extract `ContainersCard`**

```bash
cd frontend/src
mkdir -p pages
git mv FleetPage.tsx pages/FleetPage.tsx
git mv MachineDetailPage.tsx pages/MachineDetailPage.tsx
git mv Sparkline.tsx components/Sparkline.tsx
git mv ThemeToggle.tsx components/ThemeToggle.tsx
```

Cut `ContainersCard` out of `pages/MachineDetailPage.tsx` into `components/ContainersCard.tsx` (it keeps the mutation wiring from Task 3). Fix every import path in the moved files (`./api` → `../api`, `./Sparkline` → `../components/Sparkline`, etc.), and update `App.tsx` to import from `./pages/...` and render both routes inside `<AppShell>`.

- [ ] **Step 8: Restyle both pages to tokens and the new structure**

In `pages/FleetPage.tsx` and `pages/MachineDetailPage.tsx`, replace **every** hard-coded gray (`text-gray-600`, `text-gray-500`) with `text-muted-foreground`, and the six `<main className="mx-auto max-w-5xl p-8 font-sans">` wrappers with the shell's layout. Specifically:

- Fleet table: hostname cell becomes `<AssetTag tone={machineTone(row.status)}>{row.hostname}</AssetTag>` wrapped in the existing `<Link>`; add a `STATUS` column rendering `<StatusBadge tone={machineTone(row.status)} label={row.status} />` (**required** — status must never be colour-only); IP/CPU/Mem/Last-seen cells get `font-mono`; `formatLastSeen` calls become `formatRelative`.
- Render `<FleetStrip rows={rows} />` above the table.
- Machine detail: header becomes `<PageHeader title={<AssetTag …>{machine.hostname}</AssetTag>} meta={…} right={<StatusBadge …/>} />`, followed by `<Tabs tabs={[{key:"overview",label:"Overview"},{key:"containers",label:"Containers"}]} …/>` with the metrics grid under `overview` and `<ContainersCard/>` under `containers`.
- Container rows: name becomes an `AssetTag` with `containerTone(c.state)`, state text via `StatusBadge`, image/status/ids `font-mono`.

- [ ] **Step 9: Verify**

```bash
cd frontend && npx tsc --noEmit && npm run build
```
Both clean. Then `npm run dev -- --host` and check, **in both themes**: fleet (populated + empty), the strip, machine detail with both tabs, container rows in running and exited states, and the not-found/error states. Confirm no gray text remains that ignores the toggle.

- [ ] **Step 10: Commit**

```bash
git add -A frontend/src
git commit -m "feat(frontend): app shell, asset-tag screens, and the page/lib split"
```

---

### Task 5: uPlot charts

Replaces Rnui's echarts-backed `LineChart` and the hand-written SVG sparkline with one rendering path we own and theme.

**Files:**
- Modify: `frontend/package.json`
- Create: `frontend/src/components/TimeSeriesChart.tsx`
- Modify: `frontend/src/components/Sparkline.tsx`, `frontend/src/pages/MachineDetailPage.tsx`

**Interfaces:**
- Consumes: `lib/metrics.ts` series builders (Task 4).
- Produces: `<TimeSeriesChart series={{name, data, color}[]} timestamps={number[]} height?={number} />`, and a `Sparkline` with the same `{ values: number[] }` prop it has today (so `FleetPage` needs no change).

- [ ] **Step 1: Install**

```bash
cd frontend
npm install --save-exact uplot@1.6.32 uplot-react@1.2.4
```

- [ ] **Step 2: Create `frontend/src/components/TimeSeriesChart.tsx`**

uPlot needs numeric x values (seconds) and a fixed pixel size, so the component
reads its width from a resize observer and its colours from the live CSS
variables (which is what makes it follow the theme toggle).

```tsx
import { useEffect, useRef, useState } from "react";
import UplotReact from "uplot-react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";

export type ChartSeries = { name: string; data: number[]; color: string };

/** Read a CSS custom property off the document root (theme-aware). */
export function cssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v === "" ? fallback : v;
}

export default function TimeSeriesChart({
  timestamps,
  series,
  height = 200,
}: {
  timestamps: number[];
  series: ChartSeries[];
  height?: number;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  // Re-read tokens when the theme class flips so chart chrome follows the toggle.
  const [themeTick, setThemeTick] = useState(0);

  useEffect(() => {
    const el = box.current;
    if (el === null) return;
    const ro = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const mo = new MutationObserver(() => setThemeTick((n) => n + 1));
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => mo.disconnect();
  }, []);

  const axis = cssVar("--border", "#242424");
  const label = cssVar("--muted-foreground", "#8A8A8A");

  const options: uPlot.Options = {
    width: Math.max(width, 1),
    height,
    cursor: { show: true, y: false },
    legend: { show: series.length > 1 },
    scales: { x: { time: true } },
    axes: [
      {
        stroke: label,
        grid: { stroke: axis, width: 1 },
        ticks: { stroke: axis },
        font: '11px "IBM Plex Mono", monospace',
      },
      {
        stroke: label,
        grid: { stroke: axis, width: 1 },
        ticks: { stroke: axis },
        font: '11px "IBM Plex Mono", monospace',
      },
    ],
    series: [
      {},
      ...series.map((s) => ({ label: s.name, stroke: s.color, width: 2 })),
    ],
  };

  const data = [timestamps, ...series.map((s) => s.data)] as uPlot.AlignedData;

  return (
    <div ref={box} className="w-full">
      {width > 0 && <UplotReact key={`${width}-${themeTick}`} options={options} data={data} />}
    </div>
  );
}
```

- [ ] **Step 3: Rebuild `frontend/src/components/Sparkline.tsx` on uPlot**

Same public prop (`values: number[]`) so `FleetPage` is untouched. Stripped
configuration: no axes, grid, legend, cursor, or interaction.

```tsx
import UplotReact from "uplot-react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { cssVar } from "./TimeSeriesChart";

const WIDTH = 72;
const HEIGHT = 20;

/** Inline trend for a fleet row. Decorative — the numeric value sits beside it. */
export default function Sparkline({ values }: { values: number[] }) {
  if (values.length === 0) return <span className="text-muted-foreground">—</span>;

  const options: uPlot.Options = {
    width: WIDTH,
    height: HEIGHT,
    cursor: { show: false },
    legend: { show: false },
    axes: [{ show: false }, { show: false }],
    scales: { x: { time: false } },
    series: [{}, { stroke: cssVar("--chart-1", "#FFE600"), width: 1.5 }],
  };
  const data = [values.map((_, i) => i), values] as uPlot.AlignedData;

  return (
    <div aria-hidden="true">
      <UplotReact options={options} data={data} />
    </div>
  );
}
```

- [ ] **Step 4: Point `MetricChartCard` at `TimeSeriesChart`**

In `pages/MachineDetailPage.tsx`, remove `LineChart` and `LineChartSeries` from the `@e412/rnui-react` import (leaving the other components), and change `MetricChartCard` to accept `timestamps: number[]` and `series: ChartSeries[]`, rendering `<TimeSeriesChart …/>` in place of `<LineChart …/>`. Build each card's inputs from the `lib/metrics.ts` helpers, converting `ts` strings to seconds:

```tsx
const toSecs = (points: { ts: string }[]) => points.map((p) => Date.parse(p.ts) / 1000);
```

Use `--chart-1` for cpu, `--chart-2` for mem, `--chart-3` for load, and `--chart-2`/`--chart-4` for net rx/tx.

- [ ] **Step 5: Verify**

```bash
cd frontend && npx tsc --noEmit && npm run build
```
Both clean. Then `npm run dev -- --host` and confirm: all four metric charts render with data, the two-series network chart shows both rx and tx, charts resize with the window, **charts restyle when you flip the theme**, the empty-data state still shows, and fleet sparklines render in every row.

Then measure the fleet page with a realistic number of machines (the spec's density check — ~80 sparkline instances at 40 guests). If scrolling visibly degrades, report it as a concern rather than silently shipping it.

- [ ] **Step 6: Commit**

```bash
git add frontend/package.json frontend/package-lock.json frontend/src/components frontend/src/pages/MachineDetailPage.tsx
git commit -m "feat(frontend): own the chart surface with uPlot, replacing rnui LineChart"
```

---

### Task 6: Remove echarts from the bundle

**Files:**
- Create: `frontend/src/stubs/echarts.ts`
- Modify: `frontend/vite.config.ts`

**Context you need:** measured during planning — echarts + zrender are **3,105 KB of source, 55% of the bundle**. Rnui ships a single barrel that runs echarts' side-effectful `use([...])` registration at module scope, so Rollup keeps echarts *no matter what we import*; removing the `LineChart` import saved 0.68 kB. After Task 5 nothing in the app uses an Rnui chart, so echarts is genuinely dead weight and can be stubbed at the bundler.

- [ ] **Step 1: Record the before number**

```bash
cd frontend && npm run build 2>&1 | grep -E 'dist/assets'
```
Write the `index-*.js` size down; it goes in the commit message.

- [ ] **Step 2: Create `frontend/src/stubs/echarts.ts`**

```ts
/**
 * Empty stand-in for echarts.
 *
 * @e412/rnui-react ships one barrel module that does
 * `import * as core from "echarts/core"` + named imports from
 * echarts/{charts,components,renderers,features}, then runs echarts'
 * side-effectful `core.use([...])` registration at module scope. So Rollup
 * cannot tree-shake echarts out even when no chart component is imported — it is
 * ~55% of our bundle. Since we render charts with uPlot and use no rnui chart
 * component, `vite.config.ts` aliases every `echarts*` specifier to this file.
 *
 * It MUST export every name the barrel imports, or Rollup errors on a missing
 * export. The barrel calls only `use`/`registerTheme`/`init`/`graphic` on the
 * core namespace, and all of these are no-ops because chart components — which
 * would call the rest — are never rendered.
 *
 * Remove this (and the alias) once @e412/rnui-react declares
 * `"sideEffects": false` and moves the registration inside its chart module.
 */

// echarts/core namespace API — no-ops; nothing draws.
export function use(_?: unknown): void {}
export function init(): null {
  return null;
}
export function registerTheme(): void {}
export function getInstanceByDom(): null {
  return null;
}
export function connect(): void {}
export function disconnect(): void {}
export function dispose(): void {}
export const graphic = {};

// Every renderer / chart / component / feature the barrel imports by name.
// Each is an inert token; the no-op `use()` above ignores them.
const stub = {};
export const CanvasRenderer = stub;
export const BarChart = stub;
export const EffectScatterChart = stub;
export const GaugeChart = stub;
export const LineChart = stub;
export const PieChart = stub;
export const RadarChart = stub;
export const ScatterChart = stub;
export const AriaComponent = stub;
export const AxisPointerComponent = stub;
export const DataZoomComponent = stub;
export const DataZoomInsideComponent = stub;
export const DataZoomSliderComponent = stub;
export const DatasetComponent = stub;
export const GraphicComponent = stub;
export const GridComponent = stub;
export const LegendComponent = stub;
export const MarkAreaComponent = stub;
export const MarkLineComponent = stub;
export const MarkPointComponent = stub;
export const PolarComponent = stub;
export const RadarComponent = stub;
export const TimelineComponent = stub;
export const TitleComponent = stub;
export const ToolboxComponent = stub;
export const TooltipComponent = stub;
export const TransformComponent = stub;
export const VisualMapComponent = stub;
export const LabelLayout = stub;
export const UniversalTransition = stub;

export default { use, init, registerTheme, getInstanceByDom, graphic };
```

The named-export list above is derived from what `@e412/rnui-react@0.1.0`'s barrel
imports (verified during planning: `CanvasRenderer`; `Bar/EffectScatter/Gauge/
Line/Pie/Radar/Scatter Chart`; the 20 `*Component`s; `LabelLayout`,
`UniversalTransition`). If a **future rnui version** imports a new echarts symbol,
the build will fail with a clear "X is not exported by src/stubs/echarts.ts" —
add that name to the stub. That build-time error is the intended tripwire, not a
silent breakage.

- [ ] **Step 3: Alias echarts in `frontend/vite.config.ts`**

Add to the config object (keep `plugins` and `build` exactly as they are):

```ts
import { fileURLToPath, URL } from "node:url";

  resolve: {
    alias: [
      // See src/stubs/echarts.ts — drops ~55% of the bundle. Remove once rnui
      // no longer registers echarts at module scope.
      {
        find: /^echarts(\/.*)?$/,
        replacement: fileURLToPath(new URL("./src/stubs/echarts.ts", import.meta.url)),
      },
    ],
  },
```

- [ ] **Step 4: Measure and prove nothing broke**

```bash
cd frontend && npm run build 2>&1 | grep -E 'dist/assets|larger than'
JS=$(ls -S dist/assets/*.js | head -1); grep -oiF echarts "$JS" | wc -l
```
Expected: the chunk drops well under 500 kB, the Vite warning disappears, and the echarts count is `0`.

Then `npm run dev -- --host` and **exercise every screen in both themes** — fleet, machine detail with both tabs, container verbs, all four charts, sparklines. The stub throws nothing itself, so a break would surface as a blank chart or a console error; check the browser console is clean.

**If any screen breaks:** revert the alias and fall back to `manualChunks` instead, which splits echarts into its own chunk (clearing the warning without reducing bytes):

```ts
  build: {
    rollupOptions: {
      output: { manualChunks: { echarts: ["echarts"] } },
    },
  },
```
Report which lever you used and why.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/stubs frontend/vite.config.ts
git commit -m "perf(frontend): stub echarts out of the bundle

Before: <X> kB. After: <Y> kB."
```

---

### Task 7: Accessibility and final verification pass

The quality floor, checked deliberately rather than assumed.

**Files:**
- Modify: whichever components fail a check below.
- Modify: `docs/DEV.md`

- [ ] **Step 1: Status-never-colour-alone audit**

Grep every status surface and confirm each has a text label beside the colour:

```bash
cd frontend && grep -rn "AssetTag\|FleetStrip\|StatusBadge" src/
```
Fleet rows must have the `STATUS` column; container rows must show their state as text; `FleetStrip` blocks must each carry `aria-label="<hostname> — <status>"`. Fix any that don't.

- [ ] **Step 2: Contrast check on the tag fills**

In the browser devtools, verify the tag text meets 4.5:1 against each fill — `ok #00E676`, `warn #FF6D00`, `idle`, and especially `fail #FF1744`, which the spec expects to need **white** text. If any pair fails, adjust that variant's text colour in `lib/status.ts` (not the fill — fills are locked by the spec).

- [ ] **Step 3: Keyboard pass**

Tab from the fleet page through to a machine's container verb. Focus must be visible at every stop against the black ground (the `:focus-visible` rule from Task 1), tab order must be sensible, and tabs must be operable by keyboard.

- [ ] **Step 4: Narrow viewport**

At ~600px wide, tables must scroll inside their own container rather than making the page scroll horizontally. Add `overflow-x-auto` on the table wrapper where needed.

- [ ] **Step 5: Full sweep in both themes**

Fleet (populated + empty), machine detail (populated, loading, not-found, error), containers (running + exited + none), all four charts, sparklines. Both light and dark.

- [ ] **Step 6: Record it and commit**

Append a dated "Frontend design system" section to `docs/DEV.md` noting the direction, the token location (`src/index.css`), the before/after bundle numbers, and the echarts stub with its removal condition.

```bash
git add -A
git commit -m "docs: record frontend design system verification and bundle result"
```

---

## Self-Review

**Spec coverage:**
- Direction/signature (asset tag, fleet strip) — Tasks 2, 4. ✓
- Palette both themes + light text variants — Task 1. ✓
- Type (3 faces, self-hosted, mono for machine truth) — Tasks 1, 4. ✓
- `--radius: 0`, 2px rules, solid fills — Tasks 1, 4. ✓
- Shell, full-width, sticky header, tabs (Overview + Containers only) — Task 4. ✓
- File structure `app/components/lib/pages` — Tasks 2, 4. ✓
- TanStack Query replacing usePoll/loading/error/busy/refetch — Task 3. ✓
- cva variants replacing three maps — Task 2. ✓
- uPlot for charts **and** Sparkline — Task 5. ✓
- Bundle (stub, manualChunks fallback, before/after) — Task 6. ✓
- Accessibility floor — Task 7 (plus focus CSS in Task 1, aria-labels in Task 4). ✓
- Deliberately-not-added deps — Global Constraints; `formatRelative` uses `Intl`. ✓

**Deviation from the spec, deliberate:** the spec ordered "code splitting" as a lazy boundary; planning measurement showed that would not help, so Task 6 stubs echarts instead, with `manualChunks` as the documented fallback. The spec was amended to match before this plan was written.

**Placeholder scan:** no TBD/TODO; every code step carries real code; the only `<X>`/`<Y>` are measured values the implementer fills from their own build output in a commit message.

**Type consistency:** `Tone` is used identically in Tasks 2/4; `machineTone`/`containerTone` names match across `status.ts`, `FleetStrip`, and both pages; `ChartSeries` and `cssVar` are defined in `TimeSeriesChart.tsx` and imported by `Sparkline.tsx`; `qk`/`useFleet`/`useMachine`/`useMetrics`/`useDocker`/`useContainerAction` match between `queries.ts` and both pages; `Sparkline`'s `{ values: number[] }` prop is unchanged so `FleetPage` needs no edit in Task 5.
