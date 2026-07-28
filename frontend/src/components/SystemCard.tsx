// The System tab's full inventory: everything the strip doesn't have room
// for (Task 4 redesign — the strip slimmed to five items after live review
// found the original nine-item version too crowded). Pure/presentational —
// no fetching, no local state — so it's a plain function of the same
// `machine`/`resources`/`memNow` values MachineDetailPage already derives
// for the strip and the Memory chart.
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@e412/rnui-react";
import type { MachineDetail } from "../api";
import { formatBytes, formatDateTime, formatUptime } from "../lib/format";
import type { latestMem, latestResources } from "../lib/metrics";

type Row = { label: string; value: string };

/**
 * The card's ONE omission rule, as a function: a nullish or empty value
 * means no row — never a blank or an "undefined" pretending to be a fact.
 * `== null` (loose) is deliberate: these are additive proto fields, so a
 * frontend newer than its server sees `undefined`, not `null`, and strict
 * equality would let it through (this rendered "undefined · undefined
 * cores" live during this slice's own rollout).
 */
function row(label: string, value: string | null | undefined): Row | null {
  return value == null || value === "" ? null : { label, value };
}

export default function SystemCard({
  machine,
  resources,
  memNow,
}: {
  machine: MachineDetail;
  resources: ReturnType<typeof latestResources>;
  memNow: ReturnType<typeof latestMem>;
}) {
  // `formatUptime`/`formatDateTime` return "" for garbage input, and `row`
  // drops "" — so a skewed or absent boot_time simply omits both rows.
  const { disk, swap } = resources;
  const rows: Row[] = [
    row("Processor", machine.cpu_model),
    row("Cores", machine.cpu_cores?.toString()),
    row("Kernel", machine.kernel),
    row("Arch", machine.arch),
    row("Virtualization", machine.virt === "none" ? "bare metal" : machine.virt),
    row("Agent version", machine.agent_version),
    row("Boot time", machine.boot_time != null ? formatDateTime(machine.boot_time) : null),
    row("Uptime", machine.boot_time != null ? formatUptime(machine.boot_time) : null),
    row(
      "Disk",
      disk != null
        ? `${formatBytes(disk.used)} / ${formatBytes(disk.total)} (${((100 * disk.used) / disk.total).toFixed(0)}%)`
        : null,
    ),
    row("Memory", memNow != null ? formatBytes(memNow.total) : null),
    row(
      "Swap",
      swap != null && swap.total > 0
        ? `${formatBytes(swap.used)} / ${formatBytes(swap.total)}`
        : null,
    ),
    // Identity facts, always present (predate this slice).
    row("Machine ID", machine.machine_id),
    row("Enrolled", formatDateTime(machine.enrolled_at)),
  ].filter((r): r is Row => r !== null);

  return (
    <Card>
      <CardHeader>
        <CardTitle>System</CardTitle>
        <CardDescription>Inventory and resource facts reported by the agent.</CardDescription>
      </CardHeader>
      <CardContent>
        {/* Bordered surface + the strip's label treatment (SpecStrip.tsx) —
            mono uppercase muted labels — so this reads as the same family of
            fact-display as the strip above it, just laid out as rows instead
            of columns. */}
        <dl className="divide-y divide-border border border-border">
          {rows.map((row) => (
            <div
              key={row.label}
              className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-0.5 px-3 py-2"
            >
              <dt className="text-[9px] uppercase tracking-[0.16em] text-muted-foreground">
                {row.label}
              </dt>
              <dd className="font-mono text-[11px]">{row.value}</dd>
            </div>
          ))}
        </dl>
      </CardContent>
    </Card>
  );
}
