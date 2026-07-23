// Pure presentation logic for the units table, kept out of the component so it
// can be reasoned about (and tested, once a runner exists) on its own.
import type { Unit } from "../api";

/** Sort rank: failures first, then active, then everything else. */
function rank(u: Unit): number {
  if (u.active_state === "failed") return 0;
  if (u.active_state === "active") return 1;
  return 2;
}

/** How many units are in the failed state. */
export function countFailed(units: Unit[]): number {
  return units.filter((u) => u.active_state === "failed").length;
}

/**
 * The rows to render: optionally narrowed to failures, optionally filtered by a
 * case-insensitive substring of the name or description, then sorted
 * failed → active → other and alphabetically within each group.
 */
export function visibleUnits(
  units: Unit[],
  filter: string,
  failedOnly: boolean,
): Unit[] {
  const needle = filter.trim().toLowerCase();
  return units
    .filter((u) => !failedOnly || u.active_state === "failed")
    .filter(
      (u) =>
        needle === "" ||
        u.name.toLowerCase().includes(needle) ||
        u.description.toLowerCase().includes(needle),
    )
    .slice()
    .sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}
