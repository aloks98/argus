// The fleet page — the first real UI screen (Spine slice, Task 10). Polls
// GET /api/fleet and renders a table of enrolled machines with a status
// column per row, plus an amber "reconnecting…" hint for rows that were
// online/pending but have gone quiet.
import { Link } from "react-router-dom";
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
import type { FleetRow } from "../api";
import AssetTag from "../components/AssetTag";
import PageHeader from "../components/PageHeader";
import Sparkline from "../components/Sparkline";
import StatusBadge from "../components/StatusBadge";
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
    </div>
  );
}

export default function FleetPage() {
  const { data: rows = [], error, isPending } = useFleet();

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

      <div className="border-2 border-border">
        {!isPending && rows.length === 0 ? (
          <EmptyState
            title="No machines enrolled yet"
            description="Enroll an agent and it will show up here."
          />
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Hostname</TableHead>
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
                    <Link to={`/machines/${row.id}`}>
                      <AssetTag tone={machineTone(row.status)}>{row.hostname}</AssetTag>
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
                  <TableCell className="font-mono">
                    {formatRelative(row.last_seen_at)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </div>
    </>
  );
}
