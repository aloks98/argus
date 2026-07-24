// The machine detail page (Metrics slice, Task 9). Polls GET /api/machines/:id
// and GET /api/machines/:id/metrics?range=... every 10s and renders a header
// (hostname/os/ip/status/tags/last-seen) plus cpu%/mem%/load1/net-rate charts
// for the selected time range, tabbed against the container list. Mirrors
// FleetPage's polling idioms.
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import {
  Alert,
  AlertDescription,
  AlertTitle,
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
  EmptyState,
} from "@e412/rnui-react";
import ContainersCard from "../components/ContainersCard";
import LogDialog from "../components/LogDialog";
import LogFilterBar from "../components/LogFilterBar";
import LogViewer from "../components/LogViewer";
import SpecStrip from "../components/SpecStrip";
import type { SpecItem } from "../components/SpecStrip";
import StatusBadge from "../components/StatusBadge";
import Tabs from "../components/Tabs";
import TimeSeriesChart from "../components/TimeSeriesChart";
import type { ChartSeries } from "../components/TimeSeriesChart";
import UnitsCard from "../components/UnitsCard";
import { BOOT_LOGS, SYSTEM_JOURNAL } from "../api";
import { useLogFilters } from "../lib/logFilters";
import { formatBytesPerSec, formatRelative } from "../lib/format";
import {
  buildCpuSeries,
  buildLoadSeries,
  buildMemSeries,
  buildNetRateSeries,
} from "../lib/metrics";
import { useDocker, useMachine, useMetrics, useSystemd } from "../lib/queries";
import type { Range } from "../lib/queries";
import { machineTone } from "../lib/status";

const RANGES: readonly Range[] = ["1h", "6h", "24h"];

/** Declared once so the tab strip and the `?tab=` URL guard can't drift apart. */
const TABS: { key: string; label: string }[] = [
  { key: "overview", label: "Overview" },
  { key: "containers", label: "Containers" },
  { key: "units", label: "Units" },
  { key: "logs", label: "Logs" },
];
const TAB_KEYS: readonly string[] = TABS.map((t) => t.key);

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
}: {
  title: string;
  description: string;
  timestamps: number[];
  series: ChartSeries[];
  height?: number;
  format?: (v: number) => string;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      <CardContent>
        {timestamps.length === 0 ? (
          <EmptyState
            title="No data yet"
            description="Waiting for metrics to accumulate."
          />
        ) : (
          <TimeSeriesChart timestamps={timestamps} series={series} height={height} format={format} />
        )}
      </CardContent>
    </Card>
  );
}

export default function MachineDetailPage() {
  const { id } = useParams<{ id: string }>();
  const [range, setRange] = useState<Range>("1h");

  // The active tab lives in the URL (`?tab=units`) rather than component state,
  // so a reload keeps the tab and a link to "the units on this box" is
  // shareable. `replace` because switching tabs isn't a navigation step worth
  // stacking — Back should return to wherever you came from, not walk you back
  // through each tab you looked at.
  const [searchParams, setSearchParams] = useSearchParams();
  // Fall back for an unknown value too, not just a missing one: `?tab=typo`
  // would otherwise select no tab and render no panel — a blank page from a
  // hand-edited URL.
  const requestedTab = searchParams.get("tab");
  const tab = TAB_KEYS.includes(requestedTab ?? "") ? (requestedTab as string) : "overview";
  const setTab = (key: string) => {
    const next = new URLSearchParams(searchParams);
    next.set("tab", key);
    setSearchParams(next, { replace: true });
  };

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
  const error =
    machineQuery.error ?? metricsQuery.error ?? dockerQuery.error ?? systemdQuery.error;

  const backLink = (
    <Link to="/machines" className="text-sm text-muted-foreground hover:underline">
      ← Machines
    </Link>
  );

  if (id === undefined) {
    return (
      <Alert variant="destructive">
        <AlertTitle>Invalid machine id</AlertTitle>
        <AlertDescription>
          No machine id was provided in the URL.
        </AlertDescription>
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
  const memPoints = buildMemSeries(metrics);
  const loadPoints = buildLoadSeries(metrics);
  const netPoints = buildNetRateSeries(metrics);
  const latestNet =
    netPoints.length > 0 ? netPoints[netPoints.length - 1] : null;

  const specItems: SpecItem[] = [
    {
      label: "Status",
      value: <StatusBadge tone={machineTone(machine.status)} label={machine.status} />,
    },
    ...(machine.os !== null ? [{ label: "OS", value: machine.os }] : []),
    ...(machine.primary_ip !== null ? [{ label: "Address", value: machine.primary_ip }] : []),
    ...(machine.kernel !== null ? [{ label: "Kernel", value: machine.kernel }] : []),
    ...(machine.arch !== null ? [{ label: "Arch", value: machine.arch }] : []),
    ...(machine.agent_version !== null ? [{ label: "Agent", value: machine.agent_version }] : []),
    { label: "Last seen", value: formatRelative(machine.last_seen_at) },
    ...(machine.tags.length > 0 ? [{ label: "Tags", value: machine.tags.join(", ") }] : []),
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

        <h1 className="mt-2 mb-3 font-display text-2xl uppercase tracking-tight">
          {machine.hostname}
        </h1>

        <SpecStrip items={specItems} />
      </div>

      <Tabs tabs={TABS} active={tab} onChange={setTab} />

      {tab === "overview" && (
        <div
          role="tabpanel"
          id="panel-overview"
          aria-labelledby="tab-overview"
          tabIndex={0}
          className="mt-4"
        >
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
              description="Memory utilization (%)"
              timestamps={toSecs(memPoints)}
              series={[
                {
                  name: "mem %",
                  data: memPoints.map((p) => p.value),
                  colorVar: "--chart-2",
                },
              ]}
              format={formatPercent}
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
        </div>
      )}

      {tab === "containers" && (
        <div
          role="tabpanel"
          id="panel-containers"
          aria-labelledby="tab-containers"
          tabIndex={0}
          className="mt-4"
        >
          <ContainersCard machineId={id} containers={containers} />
        </div>
      )}

      {tab === "units" && (
        <div
          role="tabpanel"
          id="panel-units"
          aria-labelledby="tab-units"
          tabIndex={0}
          className="mt-4"
        >
          <UnitsCard machineId={id} units={units} />
        </div>
      )}

      {tab === "logs" && (
        <div
          role="tabpanel"
          id="panel-logs"
          aria-labelledby="tab-logs"
          tabIndex={0}
          className="mt-4 flex h-[70vh] min-h-0 flex-col"
        >
          <LogFilterBar value={logFilters} onChange={setLogFilters} />
          <div className="min-h-0 flex-1">
            <LogViewer
              machineId={id}
              source={SYSTEM_JOURNAL}
              filters={logFilters}
            />
          </div>
        </div>
      )}

      <LogDialog />
    </>
  );
}
