// systemd unit list + start/stop/restart verbs for a single machine. A host
// reports far more units than containers, so this table leads with
// failures and carries its own filter (ordering in lib/units.ts).
import { useState } from "react";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Checkbox,
  EmptyState,
  Input,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { Unit, UnitAction } from "../api";
import { describeError } from "../lib/errors";
import { useUnitAction } from "../lib/queries";
import { unitTone } from "../lib/status";
import { countFailed, isProtectedUnit, visibleUnits } from "../lib/units";
import { StatusName } from "./AssetTag";
import RowActions from "./RowActions";
import type { RowOutcome } from "./RowActions";
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

/** The last verb's outcome IF it targeted this unit — rendered inside the
 *  row so feedback appears where the click happened. Success is stated
 *  explicitly, not left implicit: the snapshot is agent-pushed on a 15s
 *  cadence, so the row's state usually hasn't changed yet, and silence
 *  after a successful click reads as "nothing happened". */
function rowOutcome(action: ReturnType<typeof useUnitAction>, unit: string): RowOutcome | null {
  const vars = action.variables;
  if (vars === undefined || vars.unit !== unit || action.isPending) return null;
  if (action.isError) return { tone: "fail", label: "failed" };
  if (action.data?.status === "pending") return { tone: "warn", label: "unconfirmed" };
  if (action.isSuccess) return { tone: "ok", label: VERB_DONE[vars.action] };
  return null;
}

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
      </div>

      {/* Success feedback lives in the acted-on ROW (RowActions' outcome
          badge), not up here — in a table this tall a banner lands off-screen
          from the click. Only the two outcomes needing more words than a
          badge keep a banner: failures (the error detail has to live
          somewhere readable) and the 202. Both ALSO mark the row. */}
      {actionError != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>
            Action failed
            {action.variables !== undefined && (
              <>
                {" — "}
                <span className="font-mono">{action.variables.unit}</span>
              </>
            )}
          </AlertTitle>
          <AlertDescription>{describeError(actionError)}</AlertDescription>
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

      {/* At this row count the table is ~250 tab stops; keyboard users need
          an exit that doesn't mean tabbing through every row. Hidden until
          focused (the sr-only → not-sr-only pattern), so pointer users never
          see it. */}
      {rows.length > 0 && (
        <a
          href="#units-table-end"
          className="sr-only border border-border px-2 py-1 font-mono text-[11px] uppercase tracking-widest focus:not-sr-only"
        >
          Skip past unit rows
        </a>
      )}

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
                {/* Hidden on phones (same reasoning as Sub/Description): the
                    name treatment still separates exception from normal
                    (StatusName: tag = warn/fail, dimmed = idle), and this
                    column's ~110px is what pushed the actions off a 390px
                    screen. */}
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
                      <StatusName tone={unitTone(u.active_state)} name={u.name} />
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
                      <RowActions
                        name={u.name}
                        logsTo={`?tab=units&logs=${encodeURIComponent(`journal:${u.name}`)}`}
                        logsDisabledReason={canReadJournal ? undefined : "no journald on this host"}
                        active={active}
                        busy={rowBusy}
                        busyLabel={
                          action.variables === undefined
                            ? "working…"
                            : VERB_PROGRESS[action.variables.action]
                        }
                        outcome={rowOutcome(action, u.name)}
                        confirmReason={
                          isProtectedUnit(u.name)
                            ? "This unit is host-critical — losing it can cut off SSH, networking, or Argus's own agent."
                            : undefined
                        }
                        onVerb={(verb) => action.mutate({ unit: u.name, action: verb })}
                      />
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </div>
      {/* The skip link's landing point — focusable by script only. */}
      <span id="units-table-end" tabIndex={-1} />
    </>
  );
}
