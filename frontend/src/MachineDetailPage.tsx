// The machine detail page (Metrics slice, Task 9). Polls GET /api/machines/:id
// and GET /api/machines/:id/metrics?range=... every 10s and renders a header
// (hostname/os/ip/status/tags/last-seen) plus cpu%/mem%/load1/net-rate charts
// for the selected time range. Mirrors FleetPage's polling + status-badge
// idioms.
import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  Button,
  ButtonGroup,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  EmptyState,
  LineChart,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { LineChartSeries } from "@e412/rnui-react";
import { containerAction, getDocker, getMachine, getMetrics } from "./api";
import type {
  Container,
  ContainerAction,
  MachineDetail,
  MetricPoint,
} from "./api";

const POLL_INTERVAL_MS = 10_000;
const RANGES = ["1h", "6h", "24h"] as const;
type Range = (typeof RANGES)[number];

type StatusBadgeVariant = "success" | "secondary" | "info" | "outline";

const STATUS_VARIANT: Record<string, StatusBadgeVariant> = {
  online: "success",
  offline: "secondary",
  pending: "info",
};

function statusVariant(status: string): StatusBadgeVariant {
  return STATUS_VARIANT[status] ?? "outline";
}

const CONTAINER_STATE_VARIANT: Record<string, StatusBadgeVariant> = {
  running: "success",
  restarting: "info",
  paused: "info",
  created: "outline",
  exited: "secondary",
  dead: "secondary",
};

function containerStateVariant(state: string): StatusBadgeVariant {
  return CONTAINER_STATE_VARIANT[state] ?? "outline";
}

function formatLastSeen(lastSeenAt: string | null): string {
  if (lastSeenAt === null) return "never";
  return new Date(lastSeenAt).toLocaleString();
}

function formatTimeLabel(ts: string): string {
  return new Date(ts).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatBytesPerSec(v: number): string {
  const units = ["B/s", "KB/s", "MB/s", "GB/s"];
  let n = v;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(1)} ${units[i]}`;
}

type ChartPoint = { ts: string; value: number };

// cpu% is reported directly by the agent.
function buildCpuSeries(points: MetricPoint[]): ChartPoint[] {
  return points
    .filter((p) => p.cpu_pct !== null)
    .map((p) => ({ ts: p.ts, value: p.cpu_pct! }));
}

// mem% is derived from the used/total counters; points missing either side
// (or with a zero total) are skipped rather than plotted as 0.
function buildMemSeries(points: MetricPoint[]): ChartPoint[] {
  return points
    .filter(
      (p) => p.mem_used !== null && p.mem_total !== null && p.mem_total > 0,
    )
    .map((p) => ({ ts: p.ts, value: (100 * p.mem_used!) / p.mem_total! }));
}

// load1 is NOT a percentage — leave it unbounded so the chart auto-scales to
// the series max instead of clamping to 0-100.
function buildLoadSeries(points: MetricPoint[]): ChartPoint[] {
  return points
    .filter((p) => p.load1 !== null)
    .map((p) => ({ ts: p.ts, value: p.load1! }));
}

type NetRatePoint = { ts: string; rx: number; tx: number };

// net_rx_bytes/net_tx_bytes are cumulative counters; derive a bytes/sec rate
// from each consecutive pair. Skip pairs with a missing counter or a
// non-positive time delta, and clamp a negative delta (counter reset) to 0.
function buildNetRateSeries(points: MetricPoint[]): NetRatePoint[] {
  const out: NetRatePoint[] = [];
  for (let i = 1; i < points.length; i++) {
    const a = points[i - 1];
    const b = points[i];
    if (
      a.net_rx_bytes === null ||
      b.net_rx_bytes === null ||
      a.net_tx_bytes === null ||
      b.net_tx_bytes === null
    ) {
      continue;
    }
    const dtSeconds = (Date.parse(b.ts) - Date.parse(a.ts)) / 1000;
    if (dtSeconds <= 0) continue;
    const rxDelta = b.net_rx_bytes - a.net_rx_bytes;
    const txDelta = b.net_tx_bytes - a.net_tx_bytes;
    out.push({
      ts: b.ts,
      rx: rxDelta < 0 ? 0 : rxDelta / dtSeconds,
      tx: txDelta < 0 ? 0 : txDelta / dtSeconds,
    });
  }
  return out;
}

function MetricChartCard({
  title,
  description,
  categories,
  series,
  height = 220,
}: {
  title: string;
  description: string;
  categories: string[];
  series: LineChartSeries[];
  height?: number;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      <CardContent>
        {categories.length === 0 ? (
          <EmptyState
            title="No data yet"
            description="Waiting for metrics to accumulate."
          />
        ) : (
          <LineChart
            categories={categories}
            series={series}
            height={height}
            showLegend={series.length > 1}
          />
        )}
      </CardContent>
    </Card>
  );
}

function ContainersCard({
  machineId,
  containers,
  onChanged,
}: {
  machineId: string;
  containers: Container[];
  onChanged: () => void;
}) {
  // container ids with a verb currently in flight -> disables those rows' buttons
  const [busy, setBusy] = useState<Set<string>>(new Set());
  const [actionError, setActionError] = useState<string | null>(null);

  async function run(container: Container, action: ContainerAction) {
    setBusy((prev) => new Set(prev).add(container.id));
    setActionError(null);
    try {
      await containerAction(machineId, container.id, action);
      onChanged();
    } catch (err) {
      setActionError(
        err instanceof Error ? err.message : `failed to ${action} ${container.name}`,
      );
    } finally {
      setBusy((prev) => {
        const next = new Set(prev);
        next.delete(container.id);
        return next;
      });
    }
  }

  return (
    <Card className="mt-6">
      <CardHeader>
        <CardTitle>Containers</CardTitle>
        <CardDescription>Docker containers on this host</CardDescription>
      </CardHeader>
      <CardContent>
        {actionError !== null && (
          <Alert variant="destructive" className="mb-4">
            <AlertTitle>Action failed</AlertTitle>
            <AlertDescription>{actionError}</AlertDescription>
          </Alert>
        )}
        {containers.length === 0 ? (
          <EmptyState
            title="No containers"
            description="This host reported no Docker containers (or has no Docker daemon)."
          />
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Image</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {containers.map((c) => {
                const running = c.state === "running";
                const rowBusy = busy.has(c.id);
                return (
                  <TableRow key={c.id}>
                    <TableCell className="font-medium">{c.name}</TableCell>
                    <TableCell className="text-gray-600">{c.image}</TableCell>
                    <TableCell>
                      <Badge variant={containerStateVariant(c.state)}>{c.state}</Badge>
                      {c.health !== "" && (
                        <Badge variant="outline" className="ml-1">
                          {c.health}
                        </Badge>
                      )}
                    </TableCell>
                    <TableCell className="text-gray-600">{c.status}</TableCell>
                    <TableCell className="text-right">
                      <ButtonGroup>
                        {running ? (
                          <>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() => void run(c, "restart")}
                            >
                              {rowBusy ? "…" : "Restart"}
                            </Button>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() => void run(c, "stop")}
                            >
                              {rowBusy ? "…" : "Stop"}
                            </Button>
                          </>
                        ) : (
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={rowBusy}
                            onClick={() => void run(c, "start")}
                          >
                            {rowBusy ? "…" : "Start"}
                          </Button>
                        )}
                      </ButtonGroup>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </CardContent>
    </Card>
  );
}

export default function MachineDetailPage() {
  const { id } = useParams<{ id: string }>();
  const [range, setRange] = useState<Range>("1h");
  const [machine, setMachine] = useState<MachineDetail | null>(null);
  const [metrics, setMetrics] = useState<MetricPoint[]>([]);
  const [containers, setContainers] = useState<Container[]>([]);
  const [loading, setLoading] = useState(true);
  const [notFound, setNotFound] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (id === undefined) return;
    let cancelled = false;

    async function poll() {
      try {
        const [machineData, metricsData, dockerData] = await Promise.all([
          getMachine(id as string),
          getMetrics(id as string, range),
          getDocker(id as string),
        ]);
        if (cancelled) return;
        setMachine(machineData);
        setMetrics(metricsData);
        setContainers(dockerData);
        setNotFound(false);
        setError(null);
      } catch (err) {
        if (cancelled) return;
        if (err instanceof Error && err.message === "machine 404") {
          setNotFound(true);
          setError(null);
        } else {
          setError(
            err instanceof Error ? err.message : "failed to load machine",
          );
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    void poll();
    const intervalId = setInterval(() => void poll(), POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(intervalId);
    };
  }, [id, range]);

  const backLink = (
    <Link to="/" className="text-sm text-gray-600 hover:underline">
      ← Fleet
    </Link>
  );

  if (id === undefined) {
    return (
      <main className="mx-auto max-w-5xl p-8 font-sans">
        <Alert variant="destructive">
          <AlertTitle>Invalid machine id</AlertTitle>
          <AlertDescription>
            No machine id was provided in the URL.
          </AlertDescription>
        </Alert>
      </main>
    );
  }

  if (notFound) {
    return (
      <main className="mx-auto max-w-5xl p-8 font-sans">
        {backLink}
        <EmptyState
          className="mt-6"
          title="Machine not found"
          description={`No machine with id "${id}" exists.`}
        />
      </main>
    );
  }

  if (loading) {
    return (
      <main className="mx-auto max-w-5xl p-8 font-sans">
        {backLink}
        <p className="mt-6 text-gray-600">Loading…</p>
      </main>
    );
  }

  if (machine === null) {
    return (
      <main className="mx-auto max-w-5xl p-8 font-sans">
        {backLink}
        <Alert variant="destructive" className="mt-6">
          <AlertTitle>Failed to load machine</AlertTitle>
          <AlertDescription>{error ?? "unknown error"}</AlertDescription>
        </Alert>
      </main>
    );
  }

  const refetchDocker = () => {
    void getDocker(id).then(setContainers).catch(() => {});
  };

  const cpuPoints = buildCpuSeries(metrics);
  const memPoints = buildMemSeries(metrics);
  const loadPoints = buildLoadSeries(metrics);
  const netPoints = buildNetRateSeries(metrics);
  const latestNet =
    netPoints.length > 0 ? netPoints[netPoints.length - 1] : null;

  return (
    <main className="mx-auto max-w-5xl p-8 font-sans">
      {backLink}

      {error !== null && (
        <Alert variant="destructive" className="mt-4">
          <AlertTitle>Failed to refresh</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card className="mt-4">
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <CardTitle className="text-2xl">{machine.hostname}</CardTitle>
              <CardDescription>
                {machine.os ?? "unknown os"} · {machine.primary_ip ?? "no ip"}
                {machine.kernel !== null ? ` · ${machine.kernel}` : ""}
                {machine.arch !== null ? ` · ${machine.arch}` : ""}
              </CardDescription>
            </div>
            <Badge variant={statusVariant(machine.status)}>
              {machine.status}
            </Badge>
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex flex-wrap gap-1">
            {machine.tags.length === 0 ? (
              <span className="text-sm text-gray-500">no tags</span>
            ) : (
              machine.tags.map((tag) => (
                <Badge key={tag} variant="outline">
                  {tag}
                </Badge>
              ))
            )}
          </div>
          <p className="mt-3 text-sm text-gray-600">
            Last seen: {formatLastSeen(machine.last_seen_at)}
            {machine.agent_version !== null &&
              ` · agent ${machine.agent_version}`}
          </p>
        </CardContent>
      </Card>

      <ContainersCard
        machineId={id}
        containers={containers}
        onChanged={refetchDocker}
      />

      <div className="mt-6 flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-lg font-semibold">Metrics</h2>
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
          categories={cpuPoints.map((p) => formatTimeLabel(p.ts))}
          series={[{ name: "cpu %", data: cpuPoints.map((p) => p.value) }]}
        />
        <MetricChartCard
          title="Memory"
          description="Memory utilization (%)"
          categories={memPoints.map((p) => formatTimeLabel(p.ts))}
          series={[{ name: "mem %", data: memPoints.map((p) => p.value) }]}
        />
        <MetricChartCard
          title="Load average"
          description="1-minute load average"
          categories={loadPoints.map((p) => formatTimeLabel(p.ts))}
          series={[{ name: "load1", data: loadPoints.map((p) => p.value) }]}
        />
        <MetricChartCard
          title="Network"
          description={
            latestNet !== null
              ? `rx/tx rate — latest rx ${formatBytesPerSec(latestNet.rx)}, tx ${formatBytesPerSec(latestNet.tx)}`
              : "rx/tx rate (bytes/sec)"
          }
          categories={netPoints.map((p) => formatTimeLabel(p.ts))}
          series={[
            { name: "rx", data: netPoints.map((p) => p.rx) },
            { name: "tx", data: netPoints.map((p) => p.tx) },
          ]}
        />
      </div>
    </main>
  );
}
