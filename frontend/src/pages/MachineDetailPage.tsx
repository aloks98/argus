// The machine detail page. Polls GET /api/machines/:id and GET
// /api/machines/:id/metrics?range=... every 10s and renders a header
// (hostname/os/ip/status/tags/last-seen) plus cpu%/mem%/load1/net-rate charts
// for the selected time range, tabbed against the container list. Mirrors
// FleetPage's polling idioms.
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
  Button,
  ButtonGroup,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  EmptyState,
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "@e412/rnui-react";
import { Pencil } from "lucide-react";
import ContainersCard from "../components/ContainersCard";
import LogDialog from "../components/LogDialog";
import LogFilterBar from "../components/LogFilterBar";
import LogViewer from "../components/LogViewer";
import MachineIdentity from "../components/MachineIdentity";
import SpecStrip from "../components/SpecStrip";
import type { SpecItem } from "../components/SpecStrip";
import StatusBadge from "../components/StatusBadge";
import SystemCard from "../components/SystemCard";
import TerminalView from "../components/TerminalView";
import TimeSeriesChart from "../components/TimeSeriesChart";
import type { ChartSeries } from "../components/TimeSeriesChart";
import UnitsCard from "../components/UnitsCard";
import { BOOT_LOGS, CAP_DOCKER, CAP_JOURNAL, CAP_SYSTEMD, SYSTEM_JOURNAL } from "../api";
import { cn } from "../lib/cn";
import { displayName } from "../lib/fleet";
import { useLogFilters } from "../lib/logFilters";
import { formatBytes, formatBytesPerSec, formatRelative, formatUptime } from "../lib/format";
import {
  buildCpuSeries,
  buildLoadSeries,
  buildMemUsedSeries,
  latestMem,
  latestResources,
  buildNetRateSeries,
} from "../lib/metrics";
import { useDocker, useMachine, useMetrics, useSystemd } from "../lib/queries";
import type { Range } from "../lib/queries";
import { machineTone } from "../lib/status";

const RANGES: readonly Range[] = ["1h", "6h", "24h"];

// uPlot wants numeric x values in seconds, not ms and not date strings.
const toSecs = (points: { ts: string }[]) => points.map((p) => Date.parse(p.ts) / 1000);

// Hoisted to module scope (rather than inline arrows) so they are
// referentially stable across renders — TimeSeriesChart memoizes its uPlot
// options keyed on `format`, and a fresh arrow every render would defeat that.
const formatPercent = (v: number) => `${v.toFixed(1)}%`;
const formatLoad = (v: number) => v.toFixed(2);

function MetricChartCard({
  title,
  description,
  timestamps,
  series,
  height = 220,
  format,
  tooltipFormat,
  rightAxisFormat,
}: {
  title: string;
  description: string;
  timestamps: number[];
  series: ChartSeries[];
  height?: number;
  format?: (v: number) => string;
  tooltipFormat?: (v: number) => string;
  rightAxisFormat?: (v: number) => string;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      <CardContent>
        {timestamps.length === 0 ? (
          <EmptyState title="No data yet" description="Waiting for metrics to accumulate." />
        ) : (
          <TimeSeriesChart
            timestamps={timestamps}
            series={series}
            height={height}
            format={format}
            tooltipFormat={tooltipFormat}
            rightAxisFormat={rightAxisFormat}
          />
        )}
      </CardContent>
    </Card>
  );
}

export default function MachineDetailPage() {
  const { id } = useParams<{ id: string }>();
  const [range, setRange] = useState<Range>("1h");

  const [editOpen, setEditOpen] = useState(false);

  // The active tab lives in the URL (`?tab=units`) rather than component state,
  // so a reload keeps the tab and a link to "the units on this box" is
  // shareable. `replace` because switching tabs isn't a navigation step worth
  // stacking — Back should return to wherever you came from, not walk you back
  // through each tab you looked at.
  const [searchParams, setSearchParams] = useSearchParams();

  // The whole journal defaults to the current boot — the cheapest and most
  // relevant read for "what has this box been doing since it came up".
  const [logFilters, setLogFilters] = useLogFilters(BOOT_LOGS);

  const machineQuery = useMachine(id as string);
  const metricsQuery = useMetrics(id as string, range);
  const dockerQuery = useDocker(id as string);
  const systemdQuery = useSystemd(id as string);

  const machine = machineQuery.data ?? null;
  const metrics = metricsQuery.data ?? [];
  const containers = dockerQuery.data ?? [];
  const units = systemdQuery.data ?? [];
  const isPending = machineQuery.isPending;
  const notFound = machineQuery.error?.message === "machine 404";
  const error = machineQuery.error ?? metricsQuery.error ?? dockerQuery.error ?? systemdQuery.error;

  // `null` capabilities means the agent predates capability reporting: gate
  // NOTHING rather than blanking a working machine. An explicit (possibly
  // empty) array is authoritative.
  const caps = machineQuery.data?.capabilities ?? null;
  const lacks = (cap: string) => caps !== null && !caps.includes(cap);
  const TABS: { key: string; label: string; disabled?: boolean; reason?: string }[] = [
    { key: "overview", label: "Overview" },
    // Not capability-gated: every machine has system facts (or the omission
    // rule renders an empty-ish card), unlike Containers/Units/Logs which
    // depend on the agent's reported capabilities.
    { key: "system", label: "System" },
    {
      key: "containers",
      label: "Containers",
      disabled: lacks(CAP_DOCKER),
      reason: lacks(CAP_DOCKER) ? "no Docker daemon on this host" : undefined,
    },
    {
      key: "units",
      label: "Units",
      disabled: lacks(CAP_SYSTEMD),
      reason: lacks(CAP_SYSTEMD) ? "no systemd on this host" : undefined,
    },
    {
      key: "logs",
      label: "Logs",
      disabled: lacks(CAP_JOURNAL),
      reason: lacks(CAP_JOURNAL) ? "no journald on this host" : undefined,
    },
    { key: "terminal", label: "Terminal" },
  ];

  // Fall back for an unknown value too, not just a missing one: `?tab=typo`
  // would otherwise select no tab and render no panel — a blank page from a
  // hand-edited URL. The same guard covers a tab that's disabled: a
  // bookmarked `?tab=units` for a host with no systemd renders the overview
  // instead of a blank panel.
  const requestedTab = searchParams.get("tab");
  const requested = TABS.find((t) => t.key === requestedTab);
  const tab = requested && !requested.disabled ? (requestedTab as string) : "overview";
  const setTab = (key: string) => {
    const next = new URLSearchParams(searchParams);
    next.set("tab", key);
    setSearchParams(next, { replace: true });
  };

  const backLink = (
    <Link to="/machines" className="text-sm text-muted-foreground hover:underline">
      ← Machines
    </Link>
  );

  if (id === undefined) {
    return (
      <Alert variant="destructive">
        <AlertTitle>Invalid machine id</AlertTitle>
        <AlertDescription>No machine id was provided in the URL.</AlertDescription>
      </Alert>
    );
  }

  if (notFound) {
    return (
      <>
        {backLink}
        <EmptyState
          className="mt-6"
          title="Machine not found"
          description={`No machine with id "${id}" exists.`}
        />
      </>
    );
  }

  if (isPending) {
    return (
      <>
        {backLink}
        <p className="mt-6 text-muted-foreground">Loading…</p>
      </>
    );
  }

  if (machine === null) {
    return (
      <>
        {backLink}
        <Alert variant="destructive" className="mt-6">
          <AlertTitle>Failed to load machine</AlertTitle>
          <AlertDescription>{error?.message ?? "unknown error"}</AlertDescription>
        </Alert>
      </>
    );
  }

  const cpuPoints = buildCpuSeries(metrics);
  const memPoints = buildMemUsedSeries(metrics);
  const memNow = latestMem(metrics);
  const resources = latestResources(metrics);
  const loadPoints = buildLoadSeries(metrics);
  const netPoints = buildNetRateSeries(metrics);
  const latestNet = netPoints.length > 0 ? netPoints[netPoints.length - 1] : null;

  // `!= null` (not `!==`): boot_time is an additive proto column, so a
  // frontend newer than the server it's talking to sees `undefined` here,
  // not `null` — strict equality would let that slip through to
  // formatUptime and render "up NaNm". Same reasoning applies wherever this
  // page and SystemCard read the inventory/resource fields added this slice.
  const uptime = machine.boot_time != null ? formatUptime(machine.boot_time) : "";

  // Five items only — Kernel/Arch/Agent/Processor/Virtualization/Disk/Memory/Swap
  // moved to the System tab (SystemCard). Uptime stays: tiny and ops-useful at a
  // glance.
  const specItems: SpecItem[] = [
    {
      label: "Status",
      value: <StatusBadge tone={machineTone(machine.status)} label={machine.status} />,
    },
    ...(machine.os !== null ? [{ label: "OS", value: machine.os }] : []),
    ...(machine.primary_ip !== null ? [{ label: "Address", value: machine.primary_ip }] : []),
    ...(uptime !== "" ? [{ label: "Uptime", value: uptime }] : []),
    { label: "Last seen", value: formatRelative(machine.last_seen_at) },
  ];

  return (
    <>
      {error != null && (
        <Alert variant="destructive" className="mt-4">
          <AlertTitle>Failed to refresh</AlertTitle>
          <AlertDescription>{error.message}</AlertDescription>
        </Alert>
      )}

      <div className="pb-3">
        <Breadcrumb>
          <BreadcrumbList className="font-mono text-[11px]">
            <BreadcrumbItem>
              <BreadcrumbLink
                render={<Link to="/machines" />}
                className="text-muted-foreground underline-offset-2 hover:underline"
              >
                Machines
              </BreadcrumbLink>
            </BreadcrumbItem>
            <BreadcrumbSeparator />
            <BreadcrumbItem>
              <BreadcrumbPage>{machine.hostname}</BreadcrumbPage>
            </BreadcrumbItem>
          </BreadcrumbList>
        </Breadcrumb>

        {/* Renamed machines show the operator-set name as the headline, with the
            hostname demoted to a muted mono line beneath — same pairing FleetPage's
            Name column uses (AssetTag + hostname), so a renamed machine reads the
            same way in the fleet table and on its detail page. Un-renamed machines
            show just the hostname. The Edit control sits in this row rather than in
            SpecStrip — editing identity isn't "a fact about the machine" the way
            OS/IP/kernel are.

            Tags live here too, as chips, rather than comma-joined in SpecStrip —
            same `Badge variant="outline"` FleetPage's Tags column uses, so a
            machine's tags read identically in both places. Omitted entirely when
            the machine has none, same as every other conditional line in this
            block. */}
        <div className="mt-2 mb-3">
          <div className="flex flex-wrap items-center gap-3">
            <h1 className="font-display text-2xl uppercase tracking-tight">
              {displayName(machine)}
            </h1>
            <Button variant="outline" size="sm" onClick={() => setEditOpen(true)}>
              <Pencil className="size-3.5" />
              Edit
            </Button>
          </div>
          {machine.display_name !== null && (
            <p className="mt-1 font-mono text-[11px] text-muted-foreground">{machine.hostname}</p>
          )}
          {machine.tags.length > 0 && (
            <div className="mt-2 flex flex-wrap gap-1">
              {machine.tags.map((tag) => (
                <Badge key={tag} variant="outline">
                  {tag}
                </Badge>
              ))}
            </div>
          )}
        </div>

        <SpecStrip items={specItems} />
      </div>

      {/* Identity editing lives in a Dialog rather than inline on the page —
          LogDialog is the in-repo precedent for a Dialog whose content depends on
          this page's own state/params. `MachineIdentity` itself is otherwise
          unchanged by this; only its host is — `onSaved` is the one seam added so
          the dialog can close itself on a successful PATCH, while staying open on
          a validation/server error so the Alert inside it is visible.

          The dialog owns the ONE heading: `MachineIdentity` no longer renders its
          own Card/CardHeader — nesting a bordered Card inside DialogContent's own
          bordered popup renders as a visible double border, and two headings
          ("Identity" from the Card, an sr-only one here) would be one too many
          anyway. */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Edit identity</DialogTitle>
            <DialogDescription>Rename, tag, and annotate this machine.</DialogDescription>
          </DialogHeader>
          <MachineIdentity machine={machine} onSaved={() => setEditOpen(false)} />
        </DialogContent>
      </Dialog>

      <Tabs value={tab} onValueChange={(value) => setTab(String(value))}>
        <TabsList>
          {TABS.map((t) => (
            <TabsTrigger
              key={t.key}
              value={t.key}
              disabled={t.disabled}
              title={t.reason}
              aria-describedby={t.disabled && t.reason ? `tab-reason-${t.key}` : undefined}
              // Only the selected colour is overridden — rnui's sizing, spacing and
              // disabled treatment are kept.
              //
              // The `dark:` copies are NOT redundant: rnui's base carries both
              // `data-active:bg-background` and `dark:data-active:bg-input/30`, and
              // tailwind-merge only dedupes classes whose modifier chains match — a
              // bare `data-active:bg-primary` beats the first but loses to the
              // second, so it would work in light mode and silently do nothing in
              // dark. The tokens already flip per mode, so the same utility is
              // correct in both; it just has to be spelled once per modifier chain
              // the library uses.
              className={cn(
                "data-active:bg-primary data-active:text-primary-foreground",
                "dark:data-active:bg-primary dark:data-active:text-primary-foreground",
                "dark:data-active:border-transparent",
              )}
            >
              {t.label}
            </TabsTrigger>
          ))}
        </TabsList>
        {/*
         * Rendered outside TabsList (a role="tablist" should contain only
         * tabs) and outside each trigger (nesting would fold the reason into
         * the trigger's accessible name and double-announce it). `title`
         * covers pointer users; this sr-only span + aria-describedby covers
         * keyboard/screen-reader users regardless of whether the library
         * renders disabled tabs with the native `disabled` attribute (which
         * suppresses `title` tooltips in Chromium and drops the control from
         * the focus order) or with `aria-disabled`.
         */}
        {TABS.map((t) =>
          t.disabled && t.reason ? (
            <span key={t.key} id={`tab-reason-${t.key}`} className="sr-only">
              {t.reason}
            </span>
          ) : null,
        )}

        <TabsContent value="overview" className="mt-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="font-display text-sm uppercase tracking-widest">Metrics</h2>
            <ButtonGroup>
              {RANGES.map((r) => (
                <Button
                  key={r}
                  variant={r === range ? "default" : "outline"}
                  size="sm"
                  onClick={() => setRange(r)}
                >
                  {r}
                </Button>
              ))}
            </ButtonGroup>
          </div>

          <div className="mt-4 grid grid-cols-1 gap-4 md:grid-cols-2">
            <MetricChartCard
              title="CPU"
              description="CPU utilization (%)"
              timestamps={toSecs(cpuPoints)}
              series={[
                {
                  name: "cpu %",
                  data: cpuPoints.map((p) => p.value),
                  colorVar: "--chart-1",
                },
              ]}
              format={formatPercent}
            />
            <MetricChartCard
              title="Memory"
              description={
                memNow !== null
                  ? `used — latest ${formatBytes(memNow.used)} of ${formatBytes(memNow.total)} (${((100 * memNow.used) / memNow.total).toFixed(0)}%)`
                  : "memory used"
              }
              timestamps={toSecs(memPoints)}
              series={[
                {
                  name: "used",
                  data: memPoints.map((p) => p.value),
                  colorVar: "--chart-2",
                },
              ]}
              format={formatBytes}
              tooltipFormat={
                memNow !== null
                  ? (v: number) => `${formatBytes(v)} (${((100 * v) / memNow.total).toFixed(0)}%)`
                  : undefined
              }
              rightAxisFormat={
                memNow !== null
                  ? (v: number) => `${((100 * v) / memNow.total).toFixed(0)}%`
                  : undefined
              }
            />
            <MetricChartCard
              title="Load average"
              description="1-minute load average"
              timestamps={toSecs(loadPoints)}
              series={[
                {
                  name: "load1",
                  data: loadPoints.map((p) => p.value),
                  colorVar: "--chart-3",
                },
              ]}
              format={formatLoad}
            />
            <MetricChartCard
              title="Network"
              description={
                latestNet !== null
                  ? `rx/tx rate — latest rx ${formatBytesPerSec(latestNet.rx)}, tx ${formatBytesPerSec(latestNet.tx)}`
                  : "rx/tx rate (bytes/sec)"
              }
              timestamps={toSecs(netPoints)}
              series={[
                {
                  name: "rx",
                  data: netPoints.map((p) => p.rx),
                  colorVar: "--chart-2",
                },
                {
                  name: "tx",
                  data: netPoints.map((p) => p.tx),
                  colorVar: "--chart-4",
                },
              ]}
              format={formatBytesPerSec}
            />
          </div>
        </TabsContent>

        <TabsContent value="system" className="mt-4">
          <SystemCard machine={machine} resources={resources} memNow={memNow} />
        </TabsContent>

        <TabsContent value="containers" className="mt-4">
          <ContainersCard machineId={id} containers={containers} />
        </TabsContent>

        <TabsContent value="units" className="mt-4">
          <UnitsCard machineId={id} units={units} canReadJournal={!lacks(CAP_JOURNAL)} />
        </TabsContent>

        <TabsContent value="logs" className="mt-4">
          {/* The sizing chain lives on this inner div, not on TabsContent. LazyLog
              is virtua-backed and derives its height from its parent, so it
              collapses to zero rows if any link in the chain does not resolve to a
              real height — putting the flex column on the panel itself renders the
              viewer but leaves it empty. */}
          <div className="flex h-[70vh] min-h-0 flex-col">
            <LogFilterBar value={logFilters} onChange={setLogFilters} />
            <div className="min-h-0 flex-1">
              <LogViewer machineId={id} source={SYSTEM_JOURNAL} filters={logFilters} />
            </div>
          </div>
        </TabsContent>

        <TabsContent value="terminal" className="mt-4">
          <TerminalView machineId={id} />
        </TabsContent>
      </Tabs>

      <LogDialog />
    </>
  );
}
