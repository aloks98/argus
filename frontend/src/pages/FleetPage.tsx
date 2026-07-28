// The fleet page — the first real UI screen (Spine slice, Task 10). Polls
// GET /api/fleet and renders a table of enrolled machines with a status
// column per row, plus an amber "reconnecting…" hint for rows that were
// online/pending but have gone quiet.
//
// Search, tag filter and group-by are all URL state (`q`, `tags`, `group`)
// so a filtered view is linkable and survives a reload — same contract as
// the Logs tab's `useLogFilters`, hand-rolled here because this page's
// params (a multi-value tag set, a single active group tag) don't fit that
// hook's shape.
import { Link, useSearchParams } from "react-router-dom";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  Button,
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
  EmptyState,
  Input,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import type { FleetRow } from "../api";
import AssetTag from "../components/AssetTag";
import PageHeader from "../components/PageHeader";
import Sparkline from "../components/Sparkline";
import StatusBadge from "../components/StatusBadge";
import { displayName, fleetTags, groupFleet, visibleFleet } from "../lib/fleet";
import { formatPct, formatRelative } from "../lib/format";
import { useFleet } from "../lib/queries";
import { machineTone } from "../lib/status";

const RECONNECT_THRESHOLD_MS = 45_000;

function isReconnecting(row: FleetRow): boolean {
  if (row.status === "offline" || row.last_seen_at === null) return false;
  return Date.now() - Date.parse(row.last_seen_at) > RECONNECT_THRESHOLD_MS;
}

function StatusCell({ row }: { row: FleetRow }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <StatusBadge tone={machineTone(row.status)} label={row.status} />
      {isReconnecting(row) && <StatusBadge tone="warn" label="reconnecting…" />}
      {row.failed_units > 0 && (
        <StatusBadge
          tone="fail"
          label={`${row.failed_units} failed unit${row.failed_units === 1 ? "" : "s"}`}
        />
      )}
    </div>
  );
}

/**
 * The table itself, extracted so the grouped view can render it once per
 * tag section rather than duplicating the markup. Callers are expected to
 * only pass non-empty `rows` — the "no machines"/"no matches" empty states
 * live at the page level, above flat vs. grouped branching.
 */
function FleetTable({ rows }: { rows: FleetRow[] }) {
  return (
    <div className="border border-border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Name</TableHead>
            <TableHead>Status</TableHead>
            <TableHead>IP</TableHead>
            <TableHead>OS</TableHead>
            <TableHead>Tags</TableHead>
            <TableHead>CPU</TableHead>
            <TableHead>CPU trend</TableHead>
            <TableHead>Mem</TableHead>
            <TableHead>Mem trend</TableHead>
            <TableHead>Last seen</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row) => (
            <TableRow key={row.id}>
              <TableCell className="font-medium">
                <Link
                  to={`/machines/${row.id}`}
                  className="flex flex-col items-start gap-0.5"
                >
                  <AssetTag tone={machineTone(row.status)}>{displayName(row)}</AssetTag>
                  {/* Only when the operator actually renamed the machine —
                      otherwise this would just repeat the AssetTag above. */}
                  {row.display_name !== null && (
                    <span className="font-mono text-[11px] text-muted-foreground">
                      {row.hostname}
                    </span>
                  )}
                </Link>
              </TableCell>
              <TableCell>
                <StatusCell row={row} />
              </TableCell>
              <TableCell className="font-mono">{row.primary_ip ?? "—"}</TableCell>
              <TableCell>{row.os ?? "—"}</TableCell>
              <TableCell>
                {row.tags.length === 0 ? (
                  "—"
                ) : (
                  <div className="flex flex-wrap gap-1">
                    {row.tags.map((tag) => (
                      <Badge key={tag} variant="outline">
                        {tag}
                      </Badge>
                    ))}
                  </div>
                )}
              </TableCell>
              <TableCell className="font-mono">{formatPct(row.cpu_pct)}</TableCell>
              <TableCell>
                <Sparkline values={row.spark_cpu} />
              </TableCell>
              <TableCell className="font-mono">{formatPct(row.mem_pct)}</TableCell>
              <TableCell>
                <Sparkline values={row.spark_mem} />
              </TableCell>
              <TableCell className="font-mono">{formatRelative(row.last_seen_at)}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

export default function FleetPage() {
  const { data: rows = [], error, isPending } = useFleet();
  const [params, setParams] = useSearchParams();

  const q = params.get("q") ?? "";
  // Tags are stored lowercase server-side and `visibleFleet` matches with an
  // exact `includes` — a hand-edited URL with stray case would otherwise
  // silently match nothing.
  const selectedTags = (params.get("tags") ?? "")
    .split(",")
    .map((t) => t.trim().toLowerCase())
    .filter(Boolean);
  // Absent/empty = flat (no grouping); otherwise the tag whose section is
  // shown in place of the flat table. Lowercased for the same reason
  // `selectedTags` is — a hand-edited URL with stray case would otherwise
  // silently match no section.
  const rawGroup = params.get("group");
  const group = rawGroup !== null && rawGroup.trim() !== "" ? rawGroup.trim().toLowerCase() : null;

  // One updater so every control writes URL state the same way; deleting
  // empty params keeps URLs canonical (absent = default, per the design).
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
  }

  function toggleTag(tag: string) {
    const next = selectedTags.includes(tag)
      ? selectedTags.filter((t) => t !== tag)
      : [...selectedTags, tag];
    setParam("tags", next.join(","));
  }

  const tags = fleetTags(rows);
  const filtered = visibleFleet(rows, q, selectedTags);
  // The group tag composes with q/tags: `groupFleet` is run on the
  // already-filtered rows, then the one section for the active group tag is
  // picked out of it — `groupFleet` still owns "a machine under every tag it
  // carries" so that semantics isn't duplicated here. A group tag that
  // matches nothing in the current filter (rather than one absent from the
  // fleet entirely — the dropdown only ever offers real tags) falls back to
  // an empty section rather than disappearing.
  const groupSection =
    group !== null ? (groupFleet(filtered).find((s) => s.tag === group) ?? { tag: group, rows: [] }) : null;

  return (
    <>
      <PageHeader
        title={
          <span className="flex flex-wrap items-baseline gap-2">
            <span>Machines</span>
            <span className="font-mono text-[11px] normal-case tracking-normal text-muted-foreground">
              {isPending
                ? "loading…"
                : `${rows.length} machine${rows.length === 1 ? "" : "s"}`}
            </span>
          </span>
        }
        meta="Machines enrolled with Argus. Refreshes every 5s."
      />

      {error != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Failed to load fleet</AlertTitle>
          <AlertDescription>{error.message}</AlertDescription>
        </Alert>
      )}

      {/* Same shape as UnitsCard's filter bar: a real `form` with
          `role="search"` and an explicit `preventDefault` — filtering is
          live on change, so Enter is a deliberate no-op rather than an
          incidental one. */}
      <form
        role="search"
        className="flex flex-wrap items-center gap-3 pb-4"
        onSubmit={(e) => e.preventDefault()}
      >
        <Input
          type="search"
          value={q}
          onChange={(e) => setParam("q", e.target.value)}
          placeholder="filter machines…"
          aria-label="Filter machines"
          className="max-w-xs font-mono text-xs"
        />
        {tags.length > 0 && (
          <div className="flex flex-wrap gap-1">
            {tags.map(({ tag }) => {
              const selected = selectedTags.includes(tag);
              return (
                // A plain `<button>` wrapping the Badge rather than Badge's own
                // `render` prop: Tailwind's preflight already strips native
                // button chrome (border-width, padding, background), so the
                // wrapper adds nothing visually but keeps the toggle semantics
                // (aria-pressed, click) independent of Badge's own prop surface.
                //
                // No count here (browser-review decision) — counts now live
                // only in the "Group by" dropdown below, so a chip is just
                // the tag itself.
                <button
                  key={tag}
                  type="button"
                  aria-pressed={selected}
                  onClick={() => toggleTag(tag)}
                >
                  <Badge variant={selected ? "default" : "outline"}>{tag}</Badge>
                </button>
              );
            })}
          </div>
        )}

        {/* Replaces the old Flat/Grouped ToggleGroup (browser-review
            decision) — a single dropdown whose trigger names the active
            group, with per-tag counts moved here (off the filter chips
            above). `DropdownMenuContent`/`DropdownMenuSubContent` already
            wrap their own Portal+Positioner (rnui's dropdown-menu.tsx), so
            no manual Portal here. */}
        <DropdownMenu>
          <DropdownMenuTrigger render={<Button variant="outline" size="sm" className="ml-auto" />}>
            {`Group by: ${group ?? "none"}`}
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onClick={() => setParam("group", "")}>None</DropdownMenuItem>
            {tags.length > 0 && (
              <DropdownMenuSub>
                <DropdownMenuSubTrigger>Tags</DropdownMenuSubTrigger>
                <DropdownMenuSubContent>
                  {tags.map(({ tag, count }) => (
                    <DropdownMenuItem key={tag} onClick={() => setParam("group", tag)}>
                      {tag} ({count})
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </form>

      {!isPending && rows.length === 0 ? (
        <div className="border border-border">
          <EmptyState
            title="No machines enrolled yet"
            description="Enroll an agent and it will show up here."
          />
        </div>
      ) : rows.length > 0 && filtered.length === 0 ? (
        <div className="border border-border">
          <EmptyState
            title="No matching machines"
            description="No machine matches the current filter."
          />
        </div>
      ) : groupSection === null ? (
        <FleetTable rows={filtered} />
      ) : (
        <div>
          <div className="flex flex-wrap items-baseline gap-2 pb-2">
            <h2 className="font-display text-sm uppercase tracking-widest">{groupSection.tag}</h2>
            <span className="font-mono text-[11px] normal-case tracking-normal text-muted-foreground">
              {groupSection.rows.length} machine{groupSection.rows.length === 1 ? "" : "s"}
            </span>
          </div>
          {groupSection.rows.length === 0 ? (
            <div className="border border-border">
              <EmptyState
                title="No matching machines"
                description={`No machine in the current filter carries the "${groupSection.tag}" tag.`}
              />
            </div>
          ) : (
            <FleetTable rows={groupSection.rows} />
          )}
        </div>
      )}
    </>
  );
}
