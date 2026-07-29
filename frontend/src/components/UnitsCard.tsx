// systemd unit list + start/stop/restart verbs for a single machine. A host
// reports far more units than containers, so this table leads with
// failures and carries its own filter (ordering in lib/units.ts).
import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  ButtonGroup,
  Checkbox,
  EmptyState,
  Input,
  Spinner,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { Unit, UnitAction } from "../api";
import { useUnitAction } from "../lib/queries";
import { unitTone } from "../lib/status";
import { countFailed, visibleUnits } from "../lib/units";
import AssetTag from "./AssetTag";
import StatusBadge from "./StatusBadge";

/** Spelled out rather than concatenated — `stop` + "ing" would read "stoping". */
const VERB_PROGRESS: Record<UnitAction, string> = {
  start: "starting…",
  stop: "stopping…",
  restart: "restarting…",
};

const VERB_DONE: Record<UnitAction, string> = {
  start: "started",
  stop: "stopped",
  restart: "restarted",
};

export default function UnitsCard({
  machineId,
  units,
  canReadJournal = true,
}: {
  machineId: string;
  units: Unit[];
  /**
   * Whether this host has journald, gated separately from the Units tab
   * (gated on `systemd`) — a host can run systemd with no readable journal,
   * and then every per-unit Logs link would open a dialog that silently
   * fails.
   */
  canReadJournal?: boolean;
}) {
  const action = useUnitAction(machineId);
  const actionError = action.error;
  const [filter, setFilter] = useState("");
  const [failedOnly, setFailedOnly] = useState(false);

  const failed = countFailed(units);
  const rows = visibleUnits(units, filter, failedOnly);

  return (
    <>
      <div className="flex flex-wrap items-baseline gap-2 pb-2">
        <h2 className="font-display text-sm uppercase tracking-widest">Units</h2>
        <span className="font-mono text-[11px] text-muted-foreground normal-case tracking-normal">
          {units.length} unit{units.length === 1 ? "" : "s"}
          {failed > 0 && ` · ${failed} failed`}
        </span>
        <span className="font-mono text-[11px] text-muted-foreground">
          systemd services on this host, plus any failed unit
        </span>
      </div>

      {/* One status line covering all three verb outcomes. Success is stated
          EXPLICITLY, not left implicit: the snapshot is agent-pushed on a 15s
          cadence, so the table usually hasn't changed yet, and silence after
          a successful click reads as "nothing happened". */}
      {actionError != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Action failed</AlertTitle>
          <AlertDescription>{actionError.message}</AlertDescription>
        </Alert>
      )}

      {/* A 202: the agent didn't report back inside the control plane's wait.
          Its own state rather than folded into success or failure — the verb may
          still be running, and claiming either would be a guess. */}
      {action.data?.status === "pending" && (
        <Alert variant="warning" className="mb-4">
          <AlertTitle>Outcome unconfirmed</AlertTitle>
          <AlertDescription>
            <span className="font-mono">{action.variables?.unit}</span> was dispatched, but the
            agent did not report a result in time. It may still be starting or stopping — this table
            refreshes as new state arrives.
          </AlertDescription>
        </Alert>
      )}

      {action.isSuccess && action.data.status !== "pending" && (
        <Alert variant="success" className="mb-4">
          <AlertTitle>
            {action.variables === undefined ? "Done" : `Unit ${VERB_DONE[action.variables.action]}`}
          </AlertTitle>
          <AlertDescription>
            <span className="font-mono">{action.variables?.unit}</span> — systemd reported the job
            completed. The row below updates on the next snapshot.
          </AlertDescription>
        </Alert>
      )}

      {/* A real `form`, not a bare div: these two controls are a search —
          `role="search"` exposes them as a landmark, and explicit
          `preventDefault` makes Enter a deliberate no-op (filtering is live
          on change) instead of a silently-broken-feeling one. `form` is a
          block box like the div was, so layout is unchanged. */}
      <form
        role="search"
        className="flex flex-wrap items-center gap-3 pb-2"
        onSubmit={(e) => e.preventDefault()}
      >
        <Input
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="filter units…"
          aria-label="Filter units by name or description"
          className="max-w-xs font-mono text-xs"
        />
        <div className="flex items-center gap-2">
          <Checkbox
            id="units-failed-only"
            checked={failedOnly}
            onCheckedChange={(checked) => setFailedOnly(checked)}
          />
          <label
            htmlFor="units-failed-only"
            className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
          >
            Failed only
          </label>
        </div>
      </form>

      {/* A tall table needs a height bound or the page scrolls, hiding the
          machine header/tabs/filter and the column headers with it. The cap
          must go on rnui's OWN `data-slot="table-container"` (already the
          scroll container via `overflow-x-auto`) — a wrapper here would
          nest a second scroll container and break `sticky` below. */}
      <div className="border border-border [&>[data-slot=table-container]]:max-h-[65vh]">
        {units.length === 0 ? (
          <EmptyState
            title="No units"
            description="This host reported no systemd units (or has no systemd)."
          />
        ) : rows.length === 0 ? (
          <EmptyState title="No matching units" description="No unit matches the current filter." />
        ) : (
          <Table>
            {/* Pinned while the body scrolls — at this row count the column
                headings are otherwise gone within one flick. The background is
                explicit so rows can't show through as they pass underneath. */}
            <TableHeader className="sticky top-0 z-10 [&_th]:bg-background">
              <TableRow>
                <TableHead>Name</TableHead>
                {/* Hidden on phones (same reasoning as Sub/Description): tone
                    already encodes active-vs-failed, and this column's
                    ~110px is what pushed Restart off a 390px screen. */}
                <TableHead className="hidden md:table-cell">Active</TableHead>
                <TableHead className="hidden md:table-cell">Sub</TableHead>
                <TableHead className="hidden md:table-cell">Description</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((u) => {
                const active = u.active_state === "active";
                const rowBusy = action.isPending && action.variables?.unit === u.name;
                return (
                  <TableRow key={u.name}>
                    {/* Unit names are unbounded — escaped device names like
                        systemd-fsck@dev-disk-by\x2dpartuuid-….service run ~90
                        chars and pushed Actions out of reach. Cap + truncate;
                        the full name is the cell's title. */}
                    <TableCell className="font-medium" title={u.name}>
                      <AssetTag
                        tone={unitTone(u.active_state)}
                        className="max-w-[16ch] md:max-w-[30ch]"
                      >
                        <span className="min-w-0 truncate">{u.name}</span>
                      </AssetTag>
                    </TableCell>
                    <TableCell className="hidden md:table-cell">
                      <StatusBadge tone={unitTone(u.active_state)} label={u.active_state} />
                    </TableCell>
                    <TableCell className="hidden md:table-cell font-mono text-muted-foreground">
                      {u.sub_state}
                    </TableCell>
                    <TableCell
                      className="hidden md:table-cell max-w-[36ch] truncate text-muted-foreground"
                      title={u.description}
                    >
                      {u.description}
                    </TableCell>
                    <TableCell className="whitespace-nowrap text-right">
                      {/* One loading state for the row while a verb runs (not a
                          spinner per button) — which verb is running is the useful
                          fact, and a job can take up to 90s. Otherwise all three
                          verbs render in the same order with inapplicable ones
                          disabled, not swapped out, so a click target never moves
                          and `title` explains why a control is disabled. */}
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
                        // `ml-auto`, not the cell's `text-right`: ButtonGroup is a
                        // block-level `flex w-fit`, so text-align doesn't move it —
                        // auto margin is what pushes a fit-width block right.
                        <ButtonGroup className="ml-auto justify-end">
                          {canReadJournal ? (
                            <Button
                              size="sm"
                              variant="outline"
                              render={
                                <Link
                                  to={`?tab=units&logs=${encodeURIComponent(`journal:${u.name}`)}`}
                                />
                              }
                              nativeButton={false}
                            >
                              Logs
                            </Button>
                          ) : (
                            // Disabled Button, not a disabled Link: `disabled` does
                            // nothing to an anchor, so a Link would still navigate to
                            // a dialog that can't load. Shown (not hidden) for
                            // consistency with Start/Stop.
                            <Button
                              size="sm"
                              variant="outline"
                              disabled
                              title="no journald on this host"
                            >
                              Logs
                            </Button>
                          )}
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={active}
                            title={active ? "Already active" : undefined}
                            onClick={() => action.mutate({ unit: u.name, action: "start" })}
                          >
                            Start
                          </Button>
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={!active}
                            title={!active ? "Not running" : undefined}
                            onClick={() => action.mutate({ unit: u.name, action: "stop" })}
                          >
                            Stop
                          </Button>
                          <Button
                            size="sm"
                            variant="outline"
                            onClick={() => action.mutate({ unit: u.name, action: "restart" })}
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
