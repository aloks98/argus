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
  failed_units: number;
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

/**
 * POST a verb and resolve only if it actually succeeded.
 *
 * The failure of a verb that *reached* the agent lives in the body, not the
 * status line: the control plane answers HTTP 200 with `ok:false` (e.g.
 * `"systemd job result: failed"` for a unit whose ExecStart died). Checking
 * only `r.ok` would render that identically to a success — which would throw
 * away the whole reason the agent waits for the real job outcome instead of
 * reporting that systemd merely accepted the request.
 *
 * So: reject on `ok:false` (the message is the agent's own), and leave
 * `ok:null` — the 202 "pending" case, where the agent hasn't answered within
 * the control plane's wait — resolved, so callers can render it as the
 * distinct *unknown* it is rather than as either outcome.
 */
async function postVerb(url: string): Promise<VerbResult> {
  const r = await fetch(url, { method: "POST" });
  // 4xx/5xx (e.g. 409 agent offline) carry no VerbResult body.
  if (!r.ok) throw new Error(`action failed: ${r.status}`);
  const result: VerbResult = await r.json();
  if (result.ok === false) {
    throw new Error(result.message ?? "the verb failed on the agent");
  }
  return result;
}

export async function containerAction(
  id: string,
  container: string,
  action: ContainerAction,
): Promise<VerbResult> {
  return postVerb(
    `/api/machines/${id}/docker/${encodeURIComponent(container)}/${action}`,
  );
}

export type Unit = {
  name: string;
  load_state: string;
  active_state: string;
  sub_state: string;
  description: string;
};

export async function getSystemd(id: string): Promise<Unit[]> {
  const r = await fetch(`/api/machines/${id}/systemd`);
  if (!r.ok) throw new Error(`systemd ${r.status}`);
  return r.json();
}

export type UnitAction = "start" | "stop" | "restart";

export async function unitAction(
  id: string,
  unit: string,
  action: UnitAction,
): Promise<VerbResult> {
  return postVerb(
    `/api/machines/${id}/units/${encodeURIComponent(unit)}/${action}`,
  );
}

/** A log source: `journal:<unit>` or `docker:<container>`. */
export type LogSource = string;

/**
 * The SSE URL for a tail. `LazyLog` opens the EventSource itself, so this
 * returns a URL rather than a fetch — see components/LogViewer.tsx.
 */
export function logStreamUrl(
  id: string,
  source: LogSource,
  tail = 200,
  follow = true,
): string {
  const params = new URLSearchParams({
    source,
    tail: String(tail),
    follow: String(follow),
  });
  return `/api/machines/${id}/logs/stream?${params.toString()}`;
}
