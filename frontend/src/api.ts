// Thin fetch wrapper around the control plane's read-only fleet endpoint
// (crates/server, Task 9). Kept dependency-free — no client library needed
// for a single GET.
export type FleetRow = {
  id: string;
  hostname: string;
  os: string | null;
  primary_ip: string | null;
  status: "pending" | "online" | "offline";
  last_seen_at: string | null;
  tags: string[];
  cpu_pct: number | null;
  mem_pct: number | null;
  spark_cpu: number[];
  spark_mem: number[];
};

export async function getFleet(): Promise<FleetRow[]> {
  const r = await fetch("/api/fleet");
  if (!r.ok) throw new Error(`fleet request failed: ${r.status}`);
  return r.json();
}

export type MachineDetail = {
  id: string;
  hostname: string;
  os: string | null;
  kernel: string | null;
  arch: string | null;
  primary_ip: string | null;
  agent_version: string | null;
  status: string;
  last_seen_at: string | null;
  enrolled_at: string;
  tags: string[];
  notes: string | null;
};

export type MetricPoint = {
  ts: string;
  cpu_pct: number | null;
  mem_used: number | null;
  mem_total: number | null;
  swap_used: number | null;
  swap_total: number | null;
  load1: number | null;
  disk_used: number | null;
  disk_total: number | null;
  net_rx_bytes: number | null;
  net_tx_bytes: number | null;
};

export async function getMachine(id: string): Promise<MachineDetail> {
  const r = await fetch(`/api/machines/${id}`);
  if (!r.ok) throw new Error(`machine ${r.status}`);
  return r.json();
}

export async function getMetrics(
  id: string,
  range: "1h" | "6h" | "24h",
): Promise<MetricPoint[]> {
  const r = await fetch(`/api/machines/${id}/metrics?range=${range}`);
  if (!r.ok) throw new Error(`metrics ${r.status}`);
  return r.json();
}

export type Container = {
  id: string;
  name: string;
  image: string;
  state: string;
  status: string;
  health: string;
};

export async function getDocker(id: string): Promise<Container[]> {
  const r = await fetch(`/api/machines/${id}/docker`);
  if (!r.ok) throw new Error(`docker ${r.status}`);
  return r.json();
}

export type ContainerAction = "start" | "stop" | "restart";

export type VerbResult = {
  command_id: string;
  ok: boolean | null;
  message: string | null;
  status: string;
};

export async function containerAction(
  id: string,
  container: string,
  action: ContainerAction,
): Promise<VerbResult> {
  const r = await fetch(
    `/api/machines/${id}/docker/${encodeURIComponent(container)}/${action}`,
    { method: "POST" },
  );
  // 200 (completed) and 202 (pending) both carry a VerbResult body; 4xx/5xx
  // (e.g. 409 agent offline) are surfaced as errors.
  if (!r.ok) throw new Error(`action failed: ${r.status}`);
  return r.json();
}
