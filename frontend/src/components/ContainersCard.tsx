// Docker container list + start/stop/restart verbs for a single machine.
// Extracted out of MachineDetailPage so the Containers tab owns its own file;
// keeps the mutation wiring (Task 3's useContainerAction) local to itself.
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  Button,
  ButtonGroup,
  EmptyState,
  Spinner,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { Container, ContainerAction } from "../api";
import { useContainerAction } from "../lib/queries";
import { containerTone } from "../lib/status";
import AssetTag from "./AssetTag";
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
        <span className="font-mono text-[11px] text-muted-foreground">
          Docker containers on this host
        </span>
      </div>

      {/* Outcome of the most recent verb — same three-state status line as
          UnitsCard, including an explicit success, since the snapshot is
          agent-pushed and the table usually hasn't caught up yet. */}
      {actionError != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Action failed</AlertTitle>
          <AlertDescription>{actionError.message}</AlertDescription>
        </Alert>
      )}

      {action.data?.status === "pending" && (
        <Alert variant="warning" className="mb-4">
          <AlertTitle>Outcome unconfirmed</AlertTitle>
          <AlertDescription>
            The command was dispatched, but the agent did not report a result in
            time. The container may still be starting or stopping — this table
            refreshes as new state arrives.
          </AlertDescription>
        </Alert>
      )}

      {action.isSuccess && action.data.status !== "pending" && (
        <Alert variant="success" className="mb-4">
          <AlertTitle>
            {action.variables === undefined
              ? "Done"
              : `Container ${VERB_DONE[action.variables.action]}`}
          </AlertTitle>
          <AlertDescription>
            The daemon reported the action completed. The row below updates on
            the next snapshot.
          </AlertDescription>
        </Alert>
      )}

      {/* rnui's `Table` brings its own overflow-x-auto container — see UnitsCard. */}
      {/* Same height bound + sticky header as UnitsCard so the two tabs behave
          alike; on a short container list the cap simply never applies. */}
      <div className="border-2 border-border [&>[data-slot=table-container]]:max-h-[65vh]">
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
                <TableHead>Image</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {containers.map((c) => {
                const running = c.state === "running";
                const rowBusy =
                  action.isPending && action.variables?.container === c.id;
                return (
                  <TableRow key={c.id}>
                    <TableCell className="font-medium">
                      <AssetTag tone={containerTone(c.state)}>{c.name}</AssetTag>
                    </TableCell>
                    <TableCell className="font-mono text-muted-foreground">
                      {c.image}
                    </TableCell>
                    <TableCell>
                      <StatusBadge tone={containerTone(c.state)} label={c.state} />
                      {c.health !== "" && (
                        <Badge variant="outline" className="ml-1 font-mono">
                          {c.health}
                        </Badge>
                      )}
                    </TableCell>
                    <TableCell className="font-mono text-muted-foreground">
                      {c.status}
                    </TableCell>
                    <TableCell className="whitespace-nowrap text-right">
                      {/* One loading state for the row rather than a "…" on
                          every button, and otherwise all three verbs with the
                          inapplicable ones disabled — kept identical to
                          UnitsCard so the two tabs behave the same way. */}
                      {rowBusy ? (
                        <span
                          role="status"
                          className="inline-flex items-center gap-2 font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
                        >
                          <Spinner className="size-3.5" />
                          {action.variables === undefined
                            ? "working…"
                            : VERB_PROGRESS[action.variables.action]}
                        </span>
                      ) : (
                        // `ml-auto` rather than the cell's `text-right` —
                        // ButtonGroup is a block-level `flex w-fit`. See UnitsCard.
                        <ButtonGroup className="ml-auto justify-end">
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={running}
                            title={running ? "Already running" : undefined}
                            onClick={() =>
                              action.mutate({ container: c.id, action: "start" })
                            }
                          >
                            Start
                          </Button>
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={!running}
                            title={!running ? "Not running" : undefined}
                            onClick={() =>
                              action.mutate({ container: c.id, action: "stop" })
                            }
                          >
                            Stop
                          </Button>
                          <Button
                            size="sm"
                            variant="outline"
                            onClick={() =>
                              action.mutate({ container: c.id, action: "restart" })
                            }
                          >
                            Restart
                          </Button>
                        </ButtonGroup>
                      )}
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
