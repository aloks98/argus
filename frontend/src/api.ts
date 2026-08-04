// Thin fetch wrapper around the control plane's HTTP API (crates/server).
// Dependency-free by design — no client library for a handful of endpoints.
import type { LogLine } from "./lib/logs";
import { Unauthenticated } from "./lib/session";

/**
 * A 401 here means the session died mid-visit, not a fetch bug — mapping it
 * to the same `Unauthenticated` type `/api/me` throws lets main.tsx's
 * QueryCache handler react uniformly instead of every poller rendering its
 * own dead-session error.
 */
function unauthenticatedOr(r: Response): Response {
  if (r.status === 401) throw new Unauthenticated();
  return r;
}

/**
 * Thrown when the server's global limiter (design §10) throttles login
 * attempts — kept distinct from a generic failure because "you're
 * rate-limited" is safe to show without revealing whether the account exists.
 */
export class RateLimited extends Error {
  retryAfterSeconds: number;
  constructor(retryAfterSeconds: number) {
    super("rate limited");
    this.name = "RateLimited";
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

/**
 * `POST /auth/local` — break-glass login (design §8). Bypasses
 * `unauthenticatedOr` on purpose: a 401 HERE means wrong credentials, not a
 * dead session, and must not resolve to `Unauthenticated`.
 *
 * Resolves on success (cookie set via `Set-Cookie`). Other failures throw a
 * deliberately generic `Error` — the server makes wrong-user/wrong-password/
 * no-admin-configured indistinguishable (design §11).
 */
export async function localLogin(username: string, password: string): Promise<void> {
  const r = await fetch("/auth/local", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ username, password }),
  });
  if (r.status === 429) {
    const parsed = Number(r.headers.get("Retry-After"));
    throw new RateLimited(Number.isFinite(parsed) && parsed > 0 ? parsed : 30);
  }
  if (!r.ok) {
    throw new Error("Sign-in failed");
  }
}

/**
 * `POST /api/local-admin/rotate` — authenticated rotation (design §5.2).
 * Behind `require_auth` like other `/api/*` verbs, so `unauthenticatedOr` is
 * correct here (unlike `localLogin`, where a 401 means bad credentials).
 *
 * Returns the new password for one-time display; never persist it beyond
 * transient UI state.
 */
export async function rotateLocalAdmin(): Promise<string> {
  const r = unauthenticatedOr(await fetch("/api/local-admin/rotate", { method: "POST" }));
  if (!r.ok) throw new Error(`rotate failed: ${r.status}`);
  const body: { password: string } = await r.json();
  return body.password;
}

/**
 * `GET /api/server-info` — fixed at control-plane boot (version is the
 * compiled-in crate version; `agent_update` is `null` when no agent binary
 * was bundled into this image). The machine page and fleet page both use
 * `agent_update` to decide whether a connected machine is running an older
 * agent than what this control plane would push.
 */
export type ServerInfo = {
  version: string;
  agent_update: { version: string; sha256: string } | null;
};

export async function getServerInfo(): Promise<ServerInfo> {
  const r = unauthenticatedOr(await fetch("/api/server-info"));
  if (!r.ok) throw new Error(`server-info ${r.status}`);
  return r.json();
}

export type FleetRow = {
  id: string;
  hostname: string;
  display_name: string | null;
  os: string | null;
  primary_ip: string | null;
  agent_version: string | null;
  status: "pending" | "online" | "offline";
  last_seen_at: string | null;
  tags: string[];
  /** `null` = the agent never reported; gate nothing. */
  capabilities: string[] | null;
  cpu_pct: number | null;
  mem_pct: number | null;
  failed_units: number;
  spark_cpu: number[];
  spark_mem: number[];
};

export async function getFleet(): Promise<FleetRow[]> {
  const r = unauthenticatedOr(await fetch("/api/fleet"));
  if (!r.ok) throw new Error(`fleet request failed: ${r.status}`);
  return r.json();
}

export type MachineDetail = {
  id: string;
  /** `/etc/machine-id` — distinct from `id` (the control plane's own row id). */
  machine_id: string;
  hostname: string;
  display_name: string | null;
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
  /** `null` = the agent never reported; gate nothing. */
  capabilities: string[] | null;
  cpu_model: string | null;
  cpu_cores: number | null;
  /** RFC3339. `null` = unreported; render nothing (never a stale/zero uptime). */
  boot_time: string | null;
  /** `"none"` is a real answer (bare metal), not an absent one. */
  virt: string | null;
};

// Capability strings the agent may report (`MachineDetail.capabilities`),
// mirroring argus_common::{CAP_SYSTEMD, CAP_DOCKER, CAP_JOURNAL} — named
// here so Rust/TS drift has exactly one place to fix.
export const CAP_SYSTEMD = "systemd";
export const CAP_DOCKER = "docker";
export const CAP_JOURNAL = "journal";

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
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}`));
  if (!r.ok) throw new Error(`machine ${r.status}`);
  return r.json();
}

export async function getMetrics(id: string, range: "1h" | "6h" | "24h"): Promise<MetricPoint[]> {
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}/metrics?range=${range}`));
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
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}/docker`));
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
 * A verb that *reached* the agent can still fail with HTTP 200 + `ok:false`
 * (the message is the agent's own) — checking only `r.ok` would hide that.
 * `ok:null` (202, still pending) resolves rather than rejects, so callers can
 * render the distinct *unknown* instead of either outcome.
 */
async function postVerb(url: string): Promise<VerbResult> {
  const r = unauthenticatedOr(await fetch(url, { method: "POST" }));
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
  return postVerb(`/api/machines/${id}/docker/${encodeURIComponent(container)}/${action}`);
}

export type Unit = {
  name: string;
  load_state: string;
  active_state: string;
  sub_state: string;
  description: string;
};

export async function getSystemd(id: string): Promise<Unit[]> {
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}/systemd`));
  if (!r.ok) throw new Error(`systemd ${r.status}`);
  return r.json();
}

export type UnitAction = "start" | "stop" | "restart";

export async function unitAction(
  id: string,
  unit: string,
  action: UnitAction,
): Promise<VerbResult> {
  return postVerb(`/api/machines/${id}/units/${encodeURIComponent(unit)}/${action}`);
}

/**
 * `POST /api/machines/:id/agent-update` — pushes the bundled agent binary to
 * this machine. Response shape matches the verb endpoints (`VerbResult`),
 * but unlike `postVerb`'s callers the 409/503 guard failures here (no
 * bundle, agent offline, unsupported arch) carry a human-readable
 * plain-text body worth surfacing verbatim, so this reads it directly
 * instead of collapsing to a generic status-code message.
 */
export async function updateAgent(id: string): Promise<VerbResult> {
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}/agent-update`, { method: "POST" }));
  if (!r.ok) {
    const text = await r.text().catch(() => "");
    throw new Error(text.trim() !== "" ? text : `agent update failed: ${r.status}`);
  }
  const result: VerbResult = await r.json();
  if (result.ok === false) {
    throw new Error(result.message ?? "the update failed on the agent");
  }
  return result;
}

/** A log source: `journal:<unit>` or `docker:<container>`. */
export type LogSource = string;

/** Time window for a journal read. Maps to journalctl `-b` / `--since`. */
export type LogWindow = "boot" | "1h" | "24h" | "all";

/** `priority` is a severity ceiling (lower = more severe); 0 means no filter. */
export type LogFilters = { priority: number; window: LogWindow };

/** Per-unit default: unfiltered, so behaviour is unchanged from before filters. */
export const ALL_LOGS: LogFilters = { priority: 0, window: "all" };
/** Whole-journal default: current boot, the cheapest and most relevant read. */
export const BOOT_LOGS: LogFilters = { priority: 0, window: "boot" };

/** The whole system journal — no `-u`. */
export const SYSTEM_JOURNAL = "journal:@system";

/**
 * Read filters out of the URL, falling back to `fallback` for anything missing
 * or invalid — the same forgiving guard `?tab=typo` gets, so a bad link renders
 * the default view instead of nothing.
 */
export function filtersFromParams(params: URLSearchParams, fallback: LogFilters): LogFilters {
  const rawPStr = params.get("priority");
  const rawP = rawPStr === null ? NaN : Number(rawPStr);
  const priority = Number.isInteger(rawP) && rawP >= 0 && rawP <= 7 ? rawP : fallback.priority;
  const rawW = params.get("window");
  const window: LogWindow =
    rawW === "boot" || rawW === "1h" || rawW === "24h" || rawW === "all" ? rawW : fallback.window;
  return { priority, window };
}

function filterParams(f: LogFilters): Record<string, string> {
  const p: Record<string, string> = { window: f.window };
  if (f.priority > 0) p.priority = String(f.priority);
  return p;
}

/**
 * The SSE URL for a tail. `LazyLog` opens the EventSource itself, so this
 * returns a URL rather than a fetch — see components/LogViewer.tsx.
 */
export function logStreamUrl(
  id: string,
  source: LogSource,
  filters: LogFilters = ALL_LOGS,
  tail = 200,
  follow = true,
): string {
  const params = new URLSearchParams({
    source,
    tail: String(tail),
    follow: String(follow),
    ...filterParams(filters),
  });
  return `/api/machines/${id}/logs/stream?${params.toString()}`;
}

export function terminalWsUrl(id: string): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}/api/machines/${id}/terminal`;
}

/** One backward page of journal entries plus the next anchor. */
export type LogPage = {
  lines: LogLine[];
  oldest_cursor: string | null;
  reached_start: boolean;
};

/**
 * Fetch the page of journal entries older than `before`. Journal only — the
 * server rejects docker sources. `before` is the oldest cursor the viewer
 * currently holds.
 */
export async function fetchLogPage(
  id: string,
  source: string,
  before: string,
  filters: LogFilters = ALL_LOGS,
  sinceMs?: number,
  limit = 500,
): Promise<LogPage> {
  const params = new URLSearchParams({
    source,
    before,
    limit: String(limit),
    ...filterParams(filters),
  });
  // The cutoff the stream resolved for this tail. Sending it keeps every page
  // on the SAME window as the tail; without it the server re-resolves `now` per
  // request and a view open longer than its own window pages into nothing.
  if (sinceMs !== undefined) params.set("since_ms", String(sinceMs));
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}/logs/page?${params.toString()}`));
  if (!r.ok) throw new Error(`log page ${r.status}`);
  return (await r.json()) as LogPage;
}

export type MachinePatchBody = {
  display_name?: string | null;
  notes?: string | null;
  tags?: string[];
};

export async function patchMachine(id: string, patch: MachinePatchBody): Promise<MachineDetail> {
  const r = unauthenticatedOr(
    await fetch(`/api/machines/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    }),
  );
  if (!r.ok) {
    const text = await r.text().catch(() => "");
    throw new Error(text.trim() !== "" ? text : `update failed: ${r.status}`);
  }
  return r.json();
}

export type EnrollmentToken = {
  id: string;
  name: string;
  display_name: string | null;
  tags: string[];
  max_uses: number | null;
  uses: number;
  expires_at: string | null;
  revoked: boolean;
  created_by: string | null;
  created_at: string;
};

export type MintTokenBody = {
  name: string;
  display_name?: string;
  tags?: string[];
  max_uses?: number | null;
  expires_in_hours?: number | null;
};

export async function listTokens(): Promise<EnrollmentToken[]> {
  const r = unauthenticatedOr(await fetch("/api/enrollment-tokens"));
  if (!r.ok) throw new Error(`tokens ${r.status}`);
  return r.json();
}

/**
 * `GET /api/enrollment-config` — the agent endpoints the server composed
 * from its own `ARGUS_AGENT_SANS` + agent port. Each is guaranteed to match
 * a SAN on the agent-surface TLS leaf, which is why the enroll block
 * interpolates one of THESE and never asks the operator to type an address:
 * a hand-typed value that "looks right" fails TLS at connect time.
 */
export type EnrollmentConfig = { agent_endpoints: string[] };

export async function getEnrollmentConfig(): Promise<EnrollmentConfig> {
  const r = unauthenticatedOr(await fetch("/api/enrollment-config"));
  if (!r.ok) throw new Error(`enrollment config ${r.status}`);
  return r.json();
}

/**
 * `GET /api/ca.pem` as text, for inlining into the enroll block's heredoc.
 * (The same PEM is public at `/ca.pem` for scripted fetches from hosts; this
 * authenticated read is for the already-signed-in enroll page.)
 */
export async function getCaPem(): Promise<string> {
  const r = unauthenticatedOr(await fetch("/api/ca.pem"));
  if (!r.ok) throw new Error(`ca.pem ${r.status}`);
  return r.text();
}

/** The `token` field is the raw secret, present ONLY in this response. */
export async function mintToken(body: MintTokenBody): Promise<EnrollmentToken & { token: string }> {
  const r = unauthenticatedOr(
    await fetch("/api/enrollment-tokens", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  );
  if (!r.ok) {
    const text = await r.text().catch(() => "");
    throw new Error(text.trim() !== "" ? text : `mint failed: ${r.status}`);
  }
  return r.json();
}

export async function revokeToken(id: string): Promise<void> {
  const r = unauthenticatedOr(await fetch(`/api/enrollment-tokens/${id}`, { method: "DELETE" }));
  if (!r.ok) throw new Error(`revoke failed: ${r.status}`);
}

export type AuditRow = {
  id: number;
  ts: string;
  actor: string;
  action: string;
  machine_id: string | null;
  hostname: string | null;
  target_ref: string | null;
  result: string | null;
  detail: Record<string, unknown>;
};

export type AuditPage = { rows: AuditRow[]; has_more: boolean };

export type AuditParams = {
  category?: string;
  machine?: string;
  result?: string;
  window?: string;
  before_id?: number;
  limit?: number;
};

export async function getAudit(params: AuditParams): Promise<AuditPage> {
  const q = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== "") q.set(key, String(value));
  }
  const qs = q.toString();
  const r = unauthenticatedOr(await fetch(`/api/audit${qs === "" ? "" : `?${qs}`}`));
  if (!r.ok) throw new Error(`audit ${r.status}`);
  return r.json();
}
