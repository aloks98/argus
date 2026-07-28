// Pure fleet-page logic: filtering, grouping, palette entries. No DOM, no
// fetch — everything here is a plain function of the fleet payload so the
// behavior is reviewable (and one day testable) without a browser.
import type { FleetRow } from "../api";
import { CAP_DOCKER, CAP_JOURNAL, CAP_SYSTEMD } from "../api";

/** The name a machine renders under everywhere: operator-set, else hostname. */
export function displayName(m: Pick<FleetRow, "display_name" | "hostname">): string {
  return m.display_name ?? m.hostname;
}

/**
 * Case-insensitive substring match on display name, hostname and tags,
 * then AND across every selected tag chip (OR across a homelab-sized fleet
 * just reads as "everything").
 */
export function visibleFleet(rows: FleetRow[], q: string, tags: string[]): FleetRow[] {
  const needle = q.trim().toLowerCase();
  return rows.filter((r) => {
    if (tags.some((t) => !r.tags.includes(t))) return false;
    if (needle === "") return true;
    return (
      displayName(r).toLowerCase().includes(needle) ||
      r.hostname.toLowerCase().includes(needle) ||
      r.tags.some((t) => t.includes(needle))
    );
  });
}

/** Every tag in the fleet, sorted, with counts — chips and autocomplete. */
export function fleetTags(rows: FleetRow[]): { tag: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const r of rows) for (const t of r.tags) counts.set(t, (counts.get(t) ?? 0) + 1);
  return [...counts.entries()]
    .map(([tag, count]) => ({ tag, count }))
    .sort((a, b) => a.tag.localeCompare(b.tag));
}

/**
 * Grouped view: one section per tag (alphabetical), a machine under EVERY
 * tag it carries (groups are views, not a partition), untagged machines
 * last under `tag: null`.
 */
export function groupFleet(rows: FleetRow[]): { tag: string | null; rows: FleetRow[] }[] {
  const sections = fleetTags(rows).map(({ tag }) => ({
    tag: tag as string | null,
    rows: rows.filter((r) => r.tags.includes(tag)),
  }));
  const untagged = rows.filter((r) => r.tags.length === 0);
  if (untagged.length > 0) sections.push({ tag: null, rows: untagged });
  return sections;
}

export type PaletteEntry = {
  /** Unique per entry — machine id, or `${id}:${tab}`. */
  key: string;
  label: string;
  /** Muted context after the label (hostname, or the machine for a tab). */
  hint: string;
  to: string;
  /** Extra match text handed to Command's filter (hostname, tags). */
  keywords: string;
};

/**
 * Machines first, then their tabs — a tab entry only exists when the
 * machine's capabilities allow it, mirroring the detail page's own gating
 * (`null` capabilities = agent never reported = gate nothing).
 */
export function paletteEntries(rows: FleetRow[]): PaletteEntry[] {
  const out: PaletteEntry[] = [];
  for (const r of rows) {
    const name = displayName(r);
    const kw = `${r.hostname} ${r.tags.join(" ")}`;
    out.push({ key: r.id, label: name, hint: r.hostname, to: `/machines/${r.id}`, keywords: kw });
    const caps = r.capabilities;
    const has = (c: string) => caps === null || caps.includes(c);
    const tabs: [string, string, boolean][] = [
      ["Containers", "containers", has(CAP_DOCKER)],
      ["Units", "units", has(CAP_SYSTEMD)],
      ["Logs", "logs", has(CAP_JOURNAL)],
      ["Terminal", "terminal", true],
    ];
    for (const [label, tab, allowed] of tabs) {
      if (!allowed) continue;
      out.push({
        key: `${r.id}:${tab}`,
        label: `${name} — ${label}`,
        hint: r.hostname,
        to: `/machines/${r.id}?tab=${tab}`,
        keywords: kw,
      });
    }
  }
  return out;
}
