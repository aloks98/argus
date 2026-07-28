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

export default function SystemCard({
  machine,
  resources,
  memNow,
}: {
  machine: MachineDetail;
  resources: ReturnType<typeof latestResources>;
  memNow: ReturnType<typeof latestMem>;
}) {
  // `!= null` (not `!==`) throughout this component is deliberate: these
  // fields are additive proto columns, so a frontend newer than the server it
  // talks to sees `undefined` for them, not `null` — strict equality would
  // let `undefined` fall through and render "undefined" in the row.
  const bootTime = machine.boot_time != null ? formatDateTime(machine.boot_time) : "";
  const uptime = machine.boot_time != null ? formatUptime(machine.boot_time) : "";

  const rows: Row[] = [
    ...(machine.cpu_model != null ? [{ label: "Processor", value: machine.cpu_model }] : []),
    ...(machine.cpu_cores != null
      ? [{ label: "Cores", value: String(machine.cpu_cores) }]
      : []),
    ...(machine.kernel != null ? [{ label: "Kernel", value: machine.kernel }] : []),
    ...(machine.arch != null ? [{ label: "Arch", value: machine.arch }] : []),
    ...(machine.virt != null
      ? [{
          label: "Virtualization",
          value: machine.virt === "none" ? "bare metal" : machine.virt,
        }]
      : []),
    ...(machine.agent_version != null
      ? [{ label: "Agent version", value: machine.agent_version }]
      : []),
    ...(bootTime !== "" ? [{ label: "Boot time", value: bootTime }] : []),
    ...(uptime !== "" ? [{ label: "Uptime", value: uptime }] : []),
    ...(resources.disk != null
      ? [{
          label: "Disk",
          value: `${formatBytes(resources.disk.used)} / ${formatBytes(resources.disk.total)} (${((100 * resources.disk.used) / resources.disk.total).toFixed(0)}%)`,
        }]
      : []),
    ...(memNow != null ? [{ label: "Memory", value: formatBytes(memNow.total) }] : []),
    ...(resources.swap != null && resources.swap.total > 0
      ? [{
          label: "Swap",
          value: `${formatBytes(resources.swap.used)} / ${formatBytes(resources.swap.total)}`,
        }]
      : []),
    // Identity facts, always present (predate this slice) — no conditional.
    { label: "Machine ID", value: machine.machine_id },
    { label: "Enrolled", value: formatDateTime(machine.enrolled_at) },
  ];

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
