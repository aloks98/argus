// Shared formatting helpers for both screens. `formatLastSeen` used to exist
// byte-identically in FleetPage and MachineDetailPage; this is its single
// home now, renamed `formatRelative` since it's no longer last-seen-specific.
const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto", style: "narrow" });

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["day", 86_400_000],
  ["hour", 3_600_000],
  ["minute", 60_000],
  ["second", 1_000],
];

/** "12s ago" style, compact. Returns "never" for a null timestamp. */
export function formatRelative(iso: string | null): string {
  if (iso === null) return "never";
  const delta = Date.parse(iso) - Date.now();
  if (!Number.isFinite(delta)) return "never";
  for (const [unit, ms] of UNITS) {
    if (Math.abs(delta) >= ms || unit === "second") {
      return rtf.format(Math.round(delta / ms), unit);
    }
  }
  return "never";
}

export function formatPct(pct: number | null): string {
  return pct == null ? "—" : `${pct.toFixed(0)}%`;
}

export function formatBytesPerSec(v: number): string {
  const units = ["B/s", "KB/s", "MB/s", "GB/s"];
  let n = v;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(1)} ${units[i]}`;
}

/** Same 1024-based ladder as `formatBytesPerSec`, for absolute sizes. */
export function formatBytes(v: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let n = v;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(1)} ${units[i]}`;
}
