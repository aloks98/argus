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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { Container } from "../api";
import { useContainerAction } from "../lib/queries";
import { containerTone } from "../lib/status";
import AssetTag from "./AssetTag";
import StatusBadge from "./StatusBadge";

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

      {actionError != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Action failed</AlertTitle>
          <AlertDescription>{actionError.message}</AlertDescription>
        </Alert>
      )}

      <div className="border-2 border-border">
        {containers.length === 0 ? (
          <EmptyState
            title="No containers"
            description="This host reported no Docker containers (or has no Docker daemon)."
          />
        ) : (
          <Table>
            <TableHeader>
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
                    <TableCell className="text-right">
                      <ButtonGroup>
                        {running ? (
                          <>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() =>
                                action.mutate({ container: c.id, action: "restart" })
                              }
                            >
                              {rowBusy ? "…" : "Restart"}
                            </Button>
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={rowBusy}
                              onClick={() =>
                                action.mutate({ container: c.id, action: "stop" })
                              }
                            >
                              {rowBusy ? "…" : "Stop"}
                            </Button>
                          </>
                        ) : (
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={rowBusy}
                            onClick={() =>
                              action.mutate({ container: c.id, action: "start" })
                            }
                          >
                            {rowBusy ? "…" : "Start"}
                          </Button>
                        )}
                      </ButtonGroup>
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
