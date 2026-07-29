// TanStack Query hooks — the single data layer for the SPA. Every screen
// consumes these instead of hand-rolling its own useEffect/setInterval poll;
// see FleetPage.tsx and MachineDetailPage.tsx for the call sites.
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  containerAction,
  getDocker,
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
 * Identity edits (display name, tags, notes) via `MachineIdentity`. Unlike
 * the verb mutations below, success writes the server's
 * refreshed `MachineDetail` straight into the cache rather than
 * invalidating: `patchMachine` already returns the authoritative post-write
 * row (server-normalized tags included), so a direct cache write skips a
 * redundant refetch. The fleet list can't be patched the same way (its rows
 * are `FleetRow`, a different shape) so that one is just invalidated — it
 * also refetches on its own 5s interval regardless.
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
 * Container verbs. The snapshot is invalidated on `onSettled`, not `onSuccess`:
 * a verb that failed on the agent still very likely changed the container's
 * state (a restart that died leaves it stopped), so the failure path is exactly
 * when a refetch matters most. Per-row in-flight state comes from `variables`,
 * which replaces the hand-rolled busy Set.
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
 * Unit verbs. Mirrors useContainerAction, including invalidating on `onSettled`
 * rather than `onSuccess` — a unit whose ExecStart failed has still moved to
 * `failed`, and that is precisely the transition the operator needs to see.
 *
 * Note the snapshot is agent-*pushed* on a 15s cadence, so this refetch often
 * returns the pre-verb state; the mutation's own error/pending result is what
 * actually tells the operator what happened, not the table.
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
