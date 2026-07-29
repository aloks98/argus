// systemd unit list + start/stop/restart verbs for a single machine. A host
// reports far more units than containers, so this table leads with failures and
// carries its own filter — see lib/units.ts for the (pure) ordering rules.
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
   * Whether this host has journald. Gated separately from the Units tab
   * itself, which is gated on `systemd`: the two are independent capabilities,
   * so a host can run systemd with no readable journal — and then every
   * per-unit Logs link would open a dialog that silently fails.
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

      {/* The outcome of the most recent verb, as one status line covering all
          three cases. Success is stated EXPLICITLY rather than left implicit:
          the snapshot is agent-pushed on a 15s cadence, so the table usually
          has not changed yet, and silence after a successful click reads as
          "nothing happened". */}
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

      {/* A real `form` rather than a bare div: these two controls are a search,
          `role="search"` exposes them as a landmark, and the explicit
          `preventDefault` makes Enter a deliberate no-op instead of an
          incidental one. Filtering is live on change, so there is nothing to
          submit — but a text field that silently swallows Enter reads as broken
          unless the element it sits in says why. Keeping the same flex classes
          means the layout is unchanged; `form` is a block box like the div was. */}
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

      {/* A tall table needs a height bound, or the page takes the scroll — the
          machine header, tabs and filter all disappear while you're reading
          rows, and the column headers go too. The cap has to go on rnui's OWN
          `data-slot="table-container"` (its `overflow-x-auto` already makes it
          the scroll container; a wrapper here would nest a second one, and
          `sticky` would then resolve against the inner container and never
          move). With the height on that element, the header below can stick
          to it. */}
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
                {/* Hidden on phones with the same reasoning as Sub/Description: the
                    AssetTag's tone already encodes active-vs-failed, and the
                    ~110px this column costs is exactly what pushed Restart off
                    a 390px screen. */}
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
                        chars and were widening the row until Actions was pushed
                        out of reach. Cap the tag and truncate; the full name is
                        the cell's title, and the row is still identifiable
                        because the escaped tail is the least distinguishing part. */}
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
                      {/* Two rules for this cell:
                          (1) While a verb runs, ONE loading state for the row —
                          not a "…" on every button. Which verb is running is the
                          useful fact, and a unit job can take up to 90s, so this
                          is on screen a while.
                          (2) Otherwise all three verbs render in the same order
                          with the inapplicable ones disabled, rather than
                          swapping which buttons exist. Buttons then sit at a
                          fixed position down the column so a click target never
                          moves between rows, and an unavailable action reads as
                          unavailable instead of missing. `title` says why — a
                          disabled control that doesn't explain itself is its own
                          puzzle. */}
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
                        // `ml-auto`, not the cell's `text-right`: ButtonGroup is
                        // a block-level `flex w-fit`, so text-align never moved
                        // it and the buttons sat left under a right-aligned
                        // header. Auto margin is what pushes a fit-width block.
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
                            // Rendered as a plain disabled Button rather than a
                            // Link: `disabled` does nothing to an anchor, so a
                            // Link here would still navigate and open a dialog
                            // that cannot load. Matches the Start/Stop buttons
                            // beside it — shown for consistency, disabled when
                            // unavailable.
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
