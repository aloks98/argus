// The audit log viewer: every action Argus took or refused, newest-first.
// Head page polls (useAudit); older pages accumulate in local state via
// keyset before_id fetches and do NOT poll. Filters are URL state, same
// contract as FleetPage.
import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  EmptyState,
  Field,
  FieldLabel,
  NativeSelect,
  NativeSelectOption,
  Spinner,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { getAudit } from "../api";
import type { AuditRow } from "../api";
import PageHeader from "../components/PageHeader";
import StatusBadge from "../components/StatusBadge";
import { AUDIT_CATEGORIES, eventLabel, resultTone } from "../lib/audit";
import { describeError } from "../lib/errors";
import { displayName } from "../lib/fleet";
import { formatRelative } from "../lib/format";
import { useAudit, useFleet } from "../lib/queries";
import type { AuditFilters } from "../lib/queries";

const WINDOWS = ["24h", "7d", "30d", "all"] as const;
const RESULTS = ["ok", "error", "denied"] as const;

function hasDetail(row: AuditRow): boolean {
  return Object.keys(row.detail).length > 0;
}

/** A UUID actor is a machine talking; `system` is the control plane itself.
 *  Both render muted — human identities carry the visual weight. */
function actorIsMachine(actor: string): boolean {
  return actor === "system" || /^[0-9a-f]{8}-[0-9a-f-]{27,}$/i.test(actor);
}

export default function AuditPage() {
  const [params, setParams] = useSearchParams();
  const filters: AuditFilters = {
    category: params.get("category") ?? "",
    machine: params.get("machine") ?? "",
    result: params.get("result") ?? "",
    window: params.get("window") ?? "",
  };
  // Serialized filter identity: older pages belong to ONE filter set, so any
  // change resets them (key on the string, not the object, for stability).
  const filterKey = `${filters.category}|${filters.machine}|${filters.result}|${filters.window}`;

  const head = useAudit(filters);
  const { data: fleet = [] } = useFleet();

  // Every head row this component has seen for the current filterKey, keyed
  // by id. The head query is a sliding window (newest ~100 by id): a burst
  // of new events shifts that window forward, and a row that has fallen off
  // the trailing edge but hasn't been reached by an `older` keyset fetch yet
  // would otherwise vanish from the merged view entirely -- present in
  // neither list. Accumulating means once a row has appeared in the head
  // page it stays rendered (until an `older` page re-supplies it). This also
  // surfaces the `result` UPDATE a verb row gets a few seconds after it's
  // created, even if the row has since slid off the head window by the time
  // that update lands: each poll's copy overwrites the previously
  // accumulated copy for the same id, so the freshest head data always wins.
  const [headSeen, setHeadSeen] = useState<{ key: string; byId: Map<number, AuditRow> } | null>(
    null,
  );

  const [older, setOlder] = useState<{ key: string; rows: AuditRow[]; hasMore: boolean } | null>(
    null,
  );
  const [olderPending, setOlderPending] = useState(false);
  const [olderError, setOlderError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<number | null>(null);

  useEffect(() => {
    const data = head.data;
    if (data === undefined) return;
    setHeadSeen((prev) => {
      const byId =
        prev !== null && prev.key === filterKey ? new Map(prev.byId) : new Map<number, AuditRow>();
      for (const row of data.rows) byId.set(row.id, row);
      return { key: filterKey, byId };
    });
  }, [head.data, filterKey]);

  function setParam(key: string, value: string) {
    setParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (value === "") next.delete(key);
        else next.set(key, value);
        return next;
      },
      { replace: true },
    );
    setOlder(null);
    setOlderError(null);
    setHeadSeen(null);
  }

  // Fall back to the raw head page while accumulation hasn't caught up yet
  // (filter just changed, effect hasn't run) -- same key-guard style as
  // `older` uses below, so back/forward navigation can't leak accumulated
  // rows from a previous filter into view.
  const headRows =
    headSeen !== null && headSeen.key === filterKey
      ? [...headSeen.byId.values()]
      : (head.data?.rows ?? []);
  const olderRows = older !== null && older.key === filterKey ? older.rows : [];
  // Merge accumulated head rows over older rows by id (head wins on the rare
  // overlap -- it's the fresher source) and re-sort to id DESC: insertion
  // order into the head map drifts from numeric order as new ids are added
  // over time, so it can't just be concatenated like before.
  const merged = new Map<number, AuditRow>();
  for (const row of olderRows) merged.set(row.id, row);
  for (const row of headRows) merged.set(row.id, row);
  const rows = [...merged.values()].sort((a, b) => b.id - a.id);
  const hasMore =
    older !== null && older.key === filterKey ? older.hasMore : (head.data?.has_more ?? false);

  async function loadOlder() {
    const oldest = rows[rows.length - 1];
    if (oldest === undefined) return;
    setOlderPending(true);
    setOlderError(null);
    try {
      const page = await getAudit({
        category: filters.category,
        machine: filters.machine,
        result: filters.result,
        window: filters.window,
        before_id: oldest.id,
      });
      setOlder({
        key: filterKey,
        rows: [...olderRows, ...page.rows],
        hasMore: page.has_more,
      });
    } catch (err) {
      setOlderError(describeError(err));
    } finally {
      setOlderPending(false);
    }
  }

  return (
    <>
      <PageHeader
        title={
          <span className="flex flex-wrap items-baseline gap-2">
            <span>Audit</span>
            <span className="font-mono text-[11px] normal-case tracking-normal text-muted-foreground">
              {head.isPending
                ? "loading…"
                : `${rows.length} event${rows.length === 1 ? "" : "s"} loaded`}
            </span>
          </span>
        }
        meta="Every action Argus took or refused. Refreshes every 10s."
      />

      {head.error != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Failed to load audit log</AlertTitle>
          <AlertDescription>{describeError(head.error)}</AlertDescription>
        </Alert>
      )}

      <form
        role="search"
        className="flex flex-wrap items-end gap-3 pb-4"
        onSubmit={(e) => e.preventDefault()}
      >
        {/* w-fit on every Field: rnui's Field is full-width by default (the
            same trap the Enroll page's endpoint select hit) — without it
            each field claims the whole row and the bar stacks vertically. */}
        <Field className="w-fit">
          <FieldLabel
            className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
            htmlFor="audit-category"
          >
            Category
          </FieldLabel>
          <NativeSelect
            id="audit-category"
            value={filters.category}
            onChange={(e) => setParam("category", e.target.value)}
          >
            <NativeSelectOption value="">all</NativeSelectOption>
            {AUDIT_CATEGORIES.map((c) => (
              <NativeSelectOption key={c} value={c}>
                {c.replace("_", " ")}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
        <Field className="w-fit">
          <FieldLabel
            className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
            htmlFor="audit-machine"
          >
            Machine
          </FieldLabel>
          <NativeSelect
            id="audit-machine"
            value={filters.machine}
            onChange={(e) => setParam("machine", e.target.value)}
          >
            <NativeSelectOption value="">all</NativeSelectOption>
            {fleet.map((m) => (
              <NativeSelectOption key={m.id} value={m.id}>
                {displayName(m)}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
        <Field className="w-fit">
          <FieldLabel
            className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
            htmlFor="audit-result"
          >
            Result
          </FieldLabel>
          <NativeSelect
            id="audit-result"
            value={filters.result}
            onChange={(e) => setParam("result", e.target.value)}
          >
            <NativeSelectOption value="">all</NativeSelectOption>
            {RESULTS.map((r) => (
              <NativeSelectOption key={r} value={r}>
                {r}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
        <Field className="w-fit">
          <FieldLabel
            className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
            htmlFor="audit-window"
          >
            Window
          </FieldLabel>
          <NativeSelect
            id="audit-window"
            value={filters.window === "" ? "7d" : filters.window}
            onChange={(e) => setParam("window", e.target.value === "7d" ? "" : e.target.value)}
          >
            {WINDOWS.map((w) => (
              <NativeSelectOption key={w} value={w}>
                {w}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
      </form>

      <div className="border border-border">
        {!head.isPending && rows.length === 0 ? (
          <EmptyState
            title="No audit events"
            description="Nothing matches the current filters and window."
          />
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                {/* Expander column */}
                <TableHead className="w-8" />
                <TableHead>Time</TableHead>
                <TableHead>Actor</TableHead>
                <TableHead>Event</TableHead>
                <TableHead className="hidden md:table-cell">Machine</TableHead>
                <TableHead className="hidden md:table-cell">Target</TableHead>
                <TableHead>Result</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((row) => (
                <AuditTableRow
                  key={row.id}
                  row={row}
                  expanded={expanded === row.id}
                  onToggle={() => setExpanded(expanded === row.id ? null : row.id)}
                />
              ))}
            </TableBody>
          </Table>
        )}
      </div>

      {olderError !== null && (
        <Alert variant="destructive" className="mt-4">
          <AlertTitle>Failed to load older events</AlertTitle>
          <AlertDescription>{olderError}</AlertDescription>
        </Alert>
      )}

      {hasMore && (
        <div className="flex justify-center pt-4">
          <Button
            variant="outline"
            size="sm"
            disabled={olderPending}
            onClick={() => void loadOlder()}
          >
            {olderPending ? (
              <>
                <Spinner className="size-3.5" />
                Loading…
              </>
            ) : (
              "Load older"
            )}
          </Button>
        </div>
      )}
    </>
  );
}

function AuditTableRow({
  row,
  expanded,
  onToggle,
}: {
  row: AuditRow;
  expanded: boolean;
  onToggle: () => void;
}) {
  const expandable = hasDetail(row);
  return (
    <>
      <TableRow>
        <TableCell className="w-8">
          {expandable && (
            <button
              type="button"
              aria-expanded={expanded}
              aria-label={expanded ? "Hide detail" : "Show detail"}
              onClick={onToggle}
              className="flex items-center text-muted-foreground hover:text-foreground"
            >
              {expanded ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
            </button>
          )}
        </TableCell>
        <TableCell className="whitespace-nowrap font-mono" title={row.ts}>
          {formatRelative(row.ts)}
        </TableCell>
        <TableCell
          className={`max-w-[18ch] truncate font-mono ${actorIsMachine(row.actor) ? "text-muted-foreground" : ""}`}
          title={row.actor}
        >
          {row.actor}
        </TableCell>
        <TableCell title={row.action}>{eventLabel(row.action)}</TableCell>
        <TableCell className="hidden md:table-cell">
          {row.machine_id !== null && row.hostname !== null ? (
            <Link
              to={`/machines/${row.machine_id}`}
              className="font-mono underline-offset-2 hover:underline"
            >
              {row.hostname}
            </Link>
          ) : (
            "—"
          )}
        </TableCell>
        <TableCell
          className="hidden md:table-cell max-w-[28ch] truncate font-mono text-muted-foreground"
          title={row.target_ref ?? undefined}
        >
          {row.target_ref ?? "—"}
        </TableCell>
        <TableCell>
          <StatusBadge tone={resultTone(row.result)} label={row.result ?? "—"} />
        </TableCell>
      </TableRow>
      {expanded && (
        <TableRow>
          {/* Full-width detail sub-row; colSpan matches the 7 columns above. */}
          <TableCell colSpan={7}>
            <pre className="overflow-x-auto p-2 font-mono text-xs text-muted-foreground">
              {JSON.stringify(row.detail, null, 2)}
            </pre>
          </TableCell>
        </TableRow>
      )}
    </>
  );
}
