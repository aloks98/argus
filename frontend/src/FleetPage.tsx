// The fleet page — the first real UI screen (Spine slice, Task 10). Polls
// GET /api/fleet and renders a table of enrolled machines with a status
// badge per row, plus an amber "reconnecting…" hint for rows that were
// online/pending but have gone quiet.
import { useEffect, useState } from "react";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Badge,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  EmptyState,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import { getFleet, type FleetRow } from "./api";

const POLL_INTERVAL_MS = 5_000;
const RECONNECT_THRESHOLD_MS = 45_000;

type StatusBadgeVariant = "success" | "secondary" | "info";

const STATUS_VARIANT: Record<FleetRow["status"], StatusBadgeVariant> = {
  online: "success",
  offline: "secondary",
  pending: "info",
};

function isReconnecting(row: FleetRow): boolean {
  if (row.status === "offline" || row.last_seen_at === null) return false;
  return Date.now() - Date.parse(row.last_seen_at) > RECONNECT_THRESHOLD_MS;
}

function formatLastSeen(lastSeenAt: string | null): string {
  if (lastSeenAt === null) return "never";
  return new Date(lastSeenAt).toLocaleString();
}

function StatusCell({ row }: { row: FleetRow }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Badge variant={STATUS_VARIANT[row.status]}>{row.status}</Badge>
      {isReconnecting(row) && <Badge variant="warning">reconnecting…</Badge>}
    </div>
  );
}

export default function FleetPage() {
  const [rows, setRows] = useState<FleetRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;

    async function poll() {
      try {
        const data = await getFleet();
        if (cancelled) return;
        setRows(data);
        setError(null);
      } catch (err) {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : "failed to load fleet");
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    void poll();
    const id = setInterval(() => void poll(), POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  return (
    <main className="mx-auto max-w-5xl p-8 font-sans">
      <h1 className="text-3xl font-semibold">Fleet</h1>
      <p className="mt-2 text-gray-600">
        Machines enrolled with Argus. Refreshes every 5s.
      </p>

      {error !== null && (
        <Alert variant="destructive" className="mt-4">
          <AlertTitle>Failed to load fleet</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card className="mt-6">
        <CardHeader>
          <CardTitle>Machines</CardTitle>
          <CardDescription>
            {loading
              ? "Loading…"
              : `${rows.length} machine${rows.length === 1 ? "" : "s"}`}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {!loading && rows.length === 0 ? (
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
                  <TableHead>Last seen</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((row) => (
                  <TableRow key={row.id}>
                    <TableCell className="font-medium">
                      {row.hostname}
                    </TableCell>
                    <TableCell>
                      <StatusCell row={row} />
                    </TableCell>
                    <TableCell>{row.primary_ip ?? "—"}</TableCell>
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
                    <TableCell>{formatLastSeen(row.last_seen_at)}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </main>
  );
}
