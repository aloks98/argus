// Chart-series builders, moved verbatim out of MachineDetailPage — they are
// pure functions over a MetricPoint[] window and don't belong to any one
// screen.
import type { MetricPoint } from "../api";

export type ChartPoint = { ts: string; value: number };

// cpu% is reported directly by the agent.
export function buildCpuSeries(points: MetricPoint[]): ChartPoint[] {
  return points
    .filter((p) => p.cpu_pct !== null)
    .map((p) => ({ ts: p.ts, value: p.cpu_pct! }));
}

// Absolute bytes used, not a derived percentage (user decision: "%" hid the
// real numbers). Total is effectively constant per machine, so the chart
// shape is identical to the old percent series — only the axis becomes
// meaningful. Points missing the counter are skipped rather than plotted as 0.
export function buildMemUsedSeries(points: MetricPoint[]): ChartPoint[] {
  return points
    .filter((p) => p.mem_used !== null)
    .map((p) => ({ ts: p.ts, value: p.mem_used! }));
}

/** The latest point carrying both counters — the card header's used/total. */
export function latestMem(points: MetricPoint[]): { used: number; total: number } | null {
  for (let i = points.length - 1; i >= 0; i--) {
    const p = points[i];
    if (p.mem_used !== null && p.mem_total !== null && p.mem_total > 0) {
      return { used: p.mem_used, total: p.mem_total };
    }
  }
  return null;
}

/**
 * The strip's Disk/Swap facts. Independent backward walks — a sample can
 * carry the disk counter pair without the swap pair or vice versa — so each
 * is found separately rather than reusing a single "latest complete point".
 */
export function latestResources(
  points: MetricPoint[],
): {
  disk: { used: number; total: number } | null;
  swap: { used: number; total: number } | null;
} {
  let disk: { used: number; total: number } | null = null;
  for (let i = points.length - 1; i >= 0; i--) {
    const p = points[i];
    if (p.disk_used !== null && p.disk_total !== null && p.disk_total > 0) {
      disk = { used: p.disk_used, total: p.disk_total };
      break;
    }
  }
  let swap: { used: number; total: number } | null = null;
  for (let i = points.length - 1; i >= 0; i--) {
    const p = points[i];
    if (p.swap_used !== null && p.swap_total !== null) {
      swap = { used: p.swap_used, total: p.swap_total };
      break;
    }
  }
  return { disk, swap };
}

// load1 is NOT a percentage — leave it unbounded so the chart auto-scales to
// the series max instead of clamping to 0-100.
export function buildLoadSeries(points: MetricPoint[]): ChartPoint[] {
  return points
    .filter((p) => p.load1 !== null)
    .map((p) => ({ ts: p.ts, value: p.load1! }));
}

export type NetRatePoint = { ts: string; rx: number; tx: number };

// net_rx_bytes/net_tx_bytes are cumulative counters; derive a bytes/sec rate
// from each consecutive pair. Skip pairs with a missing counter or a
// non-positive time delta, and clamp a negative delta (counter reset) to 0.
export function buildNetRateSeries(points: MetricPoint[]): NetRatePoint[] {
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
