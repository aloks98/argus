// Docker container list + start/stop/restart verbs for a single machine.
// Owns its own mutation wiring (`useContainerAction`).
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  EmptyState,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { Container, ContainerAction } from "../api";
import { describeError } from "../lib/errors";
import { useContainerAction } from "../lib/queries";
import { containerTone } from "../lib/status";
import { StatusName } from "./AssetTag";
import RowActions from "./RowActions";
import type { RowOutcome } from "./RowActions";
import StatusBadge from "./StatusBadge";

/** Spelled out rather than concatenated — `stop` + "ing" would read "stoping". */
const VERB_PROGRESS: Record<ContainerAction, string> = {
  start: "starting…",
  stop: "stopping…",
  restart: "restarting…",
};

const VERB_DONE: Record<ContainerAction, string> = {
  start: "started",
  stop: "stopped",
  restart: "restarted",
};

/** Same shape as UnitsCard's rowOutcome — see the comment there. Keyed on
 *  container id rather than unit name. */
function rowOutcome(
  action: ReturnType<typeof useContainerAction>,
  containerId: string,
): RowOutcome | null {
  const vars = action.variables;
  if (vars === undefined || vars.container !== containerId || action.isPending) return null;
  if (action.isError) return { tone: "fail", label: "failed" };
  if (action.data?.status === "pending") return { tone: "warn", label: "unconfirmed" };
  if (action.isSuccess) return { tone: "ok", label: VERB_DONE[vars.action] };
  return null;
}

export default function ContainersCard({
  machineId,
  containers,
}: {
  machineId: string;
  containers: Container[];
}) {
  const action = useContainerAction(machineId);
  const actionError = action.error;

  return (
    <>
      <div className="flex flex-wrap items-baseline gap-2 pb-2">
        <h2 className="font-display text-sm uppercase tracking-widest">Containers</h2>
        <span className="font-mono text-[11px] text-muted-foreground normal-case tracking-normal">
          {containers.length} container{containers.length === 1 ? "" : "s"}
        </span>
      </div>

      {/* Success feedback lives in the acted-on ROW (RowActions' outcome
          badge) — same reasoning as UnitsCard. Only failures and the 202
          keep a banner; both also mark the row. */}
      {actionError != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Action failed</AlertTitle>
          <AlertDescription>{describeError(actionError)}</AlertDescription>
        </Alert>
      )}

      {action.data?.status === "pending" && (
        <Alert variant="warning" className="mb-4">
          <AlertTitle>Outcome unconfirmed</AlertTitle>
          <AlertDescription>
            The command was dispatched, but the agent did not report a result in time. The container
            may still be starting or stopping — this table refreshes as new state arrives.
          </AlertDescription>
        </Alert>
      )}

      {/* rnui's `Table` brings its own overflow-x-auto container — see UnitsCard. */}
      {/* Same height bound + sticky header as UnitsCard so the two tabs behave
          alike; on a short container list the cap simply never applies. */}
      <div className="border border-border [&>[data-slot=table-container]]:max-h-[65vh]">
        {containers.length === 0 ? (
          <EmptyState
            title="No containers"
            description="This host reported no Docker containers (or has no Docker daemon)."
          />
        ) : (
          <Table>
            <TableHeader className="sticky top-0 z-10 [&_th]:bg-background">
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead className="hidden md:table-cell">Image</TableHead>
                {/* Same phone-width trade as UnitsCard's Active column: the
                    name treatment separates exception from normal (StatusName),
                    and the column's width is what keeps the actions reachable
                    at 390px. */}
                <TableHead className="hidden md:table-cell">State</TableHead>
                <TableHead className="hidden md:table-cell">Status</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {containers.map((c) => {
                const running = c.state === "running";
                const rowBusy = action.isPending && action.variables?.container === c.id;
                return (
                  <TableRow key={c.id}>
                    <TableCell className="font-medium" title={c.name}>
                      <StatusName tone={containerTone(c.state)} name={c.name} />
                    </TableCell>
                    <TableCell className="hidden md:table-cell font-mono text-muted-foreground">
                      {c.image}
                    </TableCell>
                    <TableCell className="hidden md:table-cell">
                      <StatusBadge tone={containerTone(c.state)} label={c.state} />
                      {c.health !== "" && (
                        <Badge variant="outline" className="ml-1 font-mono">
                          {c.health}
                        </Badge>
                      )}
                    </TableCell>
                    <TableCell className="hidden md:table-cell font-mono text-muted-foreground">
                      {c.status}
                    </TableCell>
                    <TableCell className="whitespace-nowrap text-right">
                      <RowActions
                        name={c.name}
                        logsTo={`?tab=containers&logs=${encodeURIComponent(`docker:${c.id}`)}`}
                        active={running}
                        busy={rowBusy}
                        busyLabel={
                          action.variables === undefined
                            ? "working…"
                            : VERB_PROGRESS[action.variables.action]
                        }
                        outcome={rowOutcome(action, c.id)}
                        onVerb={(verb) => action.mutate({ container: c.id, action: verb })}
                      />
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </div>
    </>
  );
}
