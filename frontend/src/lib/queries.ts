// TanStack Query hooks — the single data layer for the SPA; every screen
// consumes these instead of hand-rolling its own useEffect/setInterval poll.
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  containerAction,
  getCaPem,
  getAudit,
  getDocker,
  getEnrollmentConfig,
  getFleet,
  getMachine,
  getMetrics,
  getSystemd,
  listTokens,
  mintToken,
  patchMachine,
  revokeToken,
  unitAction,
} from "../api";
import type { ContainerAction, MachinePatchBody, MintTokenBody, UnitAction } from "../api";

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
  systemd: (id: string) => ["systemd", id] as const,
  enrollmentTokens: ["enrollment-tokens"] as const,
  enrollmentConfig: ["enrollment-config"] as const,
  caPem: ["ca-pem"] as const,
  audit: (f: AuditFilters) => ["audit", f.category, f.machine, f.result, f.window] as const,
};

/**
 * `enabled` defaults to `true` (the fleet page's normal always-on poll); the
 * command palette passes `enabled: open` so mounting it app-wide doesn't add
 * a permanent background poll to pages that don't otherwise need the fleet.
 */
export function useFleet(options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: qk.fleet,
    queryFn: getFleet,
    refetchInterval: FLEET_INTERVAL,
    enabled: options?.enabled ?? true,
  });
}

export function useMachine(id: string) {
  return useQuery({
    queryKey: qk.machine(id),
    queryFn: () => getMachine(id),
    refetchInterval: MACHINE_INTERVAL,
  });
}

/**
 * Success writes the server's response straight into the `machine` cache
 * (`patchMachine` returns the authoritative post-write row, so a direct
 * write skips a redundant refetch) but only invalidates `fleet` — its rows
 * are a different shape (`FleetRow`), and it refetches on its own 5s poll.
 */
export function useUpdateMachine(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: MachinePatchBody) => patchMachine(id, patch),
    onSuccess: (data) => {
      qc.setQueryData(qk.machine(id), data);
      void qc.invalidateQueries({ queryKey: qk.fleet });
    },
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
 * Invalidated on `onSettled`, not `onSuccess`: a verb that failed on the
 * agent still very likely changed state (a restart that died leaves it
 * stopped) — the failure path is exactly when a refetch matters most.
 */
export function useContainerAction(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { container: string; action: ContainerAction }) =>
      containerAction(id, vars.container, vars.action),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: qk.docker(id) });
    },
  });
}

export function useSystemd(id: string) {
  return useQuery({
    queryKey: qk.systemd(id),
    queryFn: () => getSystemd(id),
    refetchInterval: MACHINE_INTERVAL,
  });
}

/**
 * Mirrors useContainerAction (invalidate on `onSettled`, not `onSuccess`).
 * Unlike containers, the snapshot is agent-*pushed* on a 15s cadence, so
 * this refetch often returns pre-verb state — the mutation's own
 * error/pending result is what tells the operator what happened.
 */
export function useUnitAction(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { unit: string; action: UnitAction }) =>
      unitAction(id, vars.unit, vars.action),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: qk.systemd(id) });
    },
  });
}

/**
 * Both feed the enroll block. No `refetchInterval` and `staleTime: Infinity`:
 * the agent endpoints and CA cert are fixed at control-plane boot (SANs from
 * env, CA generated once and persisted), so refetching within a session can
 * never observe a change.
 */
export function useEnrollmentConfig() {
  return useQuery({
    queryKey: qk.enrollmentConfig,
    queryFn: getEnrollmentConfig,
    staleTime: Infinity,
  });
}

export function useCaPem() {
  return useQuery({
    queryKey: qk.caPem,
    queryFn: getCaPem,
    staleTime: Infinity,
  });
}

/**
 * Enrollment tokens for the `/enroll` page. No `refetchInterval` — unlike
 * the fleet/machine polls, this list only changes in response to this same
 * page's own mint/revoke mutations, which already invalidate it below.
 */
export function useEnrollmentTokens() {
  return useQuery({
    queryKey: qk.enrollmentTokens,
    queryFn: listTokens,
  });
}

/**
 * The raw token in the response is deliberately NOT written anywhere here —
 * only the caller's local component state holds it (see `EnrollPage`'s
 * result panel) — it must be shown once only.
 */
export function useMintToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: MintTokenBody) => mintToken(body),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: qk.enrollmentTokens });
    },
  });
}

export function useRevokeToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => revokeToken(id),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: qk.enrollmentTokens });
    },
  });
}

/** URL-state filter values; empty string = unset (matches the URL contract). */
export type AuditFilters = {
  category: string;
  machine: string;
  result: string;
  window: string;
};

/**
 * The audit page's HEAD query: first page only, polled — which also picks up
 * the seconds-later `result` UPDATE that verb rows receive. Older pages are
 * fetched imperatively by the page (they don't poll; cost stays flat).
 */
export function useAudit(filters: AuditFilters) {
  return useQuery({
    queryKey: qk.audit(filters),
    queryFn: () =>
      getAudit({
        category: filters.category,
        machine: filters.machine,
        result: filters.result,
        window: filters.window,
      }),
    refetchInterval: MACHINE_INTERVAL,
  });
}
