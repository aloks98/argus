// Thin fetch wrapper around the control plane's read-only fleet endpoint
// (crates/server, Task 9). Kept dependency-free — no client library needed
// for a single GET.
export type FleetRow = {
  id: string;
  hostname: string;
  os: string | null;
  primary_ip: string | null;
  status: "pending" | "online" | "offline";
  last_seen_at: string | null;
  tags: string[];
};

export async function getFleet(): Promise<FleetRow[]> {
  const r = await fetch("/api/fleet");
  if (!r.ok) throw new Error(`fleet request failed: ${r.status}`);
  return r.json();
}
