// TanStack Query hooks — the single data layer for the SPA. Every screen
// consumes these instead of hand-rolling its own useEffect/setInterval poll;
// see FleetPage.tsx and MachineDetailPage.tsx for the call sites.
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  containerAction,
  getDocker,
  getFleet,
  getMachine,
  getMetrics,
} from "../api";
import type { ContainerAction } from "../api";

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
};

export function useFleet() {
  return useQuery({
    queryKey: qk.fleet,
    queryFn: getFleet,
    refetchInterval: FLEET_INTERVAL,
  });
}

export function useMachine(id: string) {
  return useQuery({
    queryKey: qk.machine(id),
    queryFn: () => getMachine(id),
    refetchInterval: MACHINE_INTERVAL,
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
 * Container verbs. On success the docker snapshot is invalidated so the panel
 * reflects the new state without waiting for the next poll — this replaces the
 * manual refetchDocker(). Per-row in-flight state comes from `variables`, which
 * replaces the hand-rolled busy Set.
 */
export function useContainerAction(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { container: string; action: ContainerAction }) =>
      containerAction(id, vars.container, vars.action),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: qk.docker(id) });
    },
  });
}
