// Pure presentation logic for the units table, kept out of the component so it
// can be reasoned about (and tested, once a runner exists) on its own.
import type { Unit } from "../api";

function rank(u: Unit): number {
  if (u.active_state === "failed") return 0;
  if (u.active_state === "active") return 1;
  return 2;
}

export function countFailed(units: Unit[]): number {
  return units.filter((u) => u.active_state === "failed").length;
}

/**
 * Units where a mis-click on Stop/Restart costs more than the unit itself:
 * the operator's way in (ssh), the host's network path, the runtime every
 * container sits on, or Argus's own eyes on the machine. Verbs on these
 * confirm first (RowActions); everything else stays one click — the guard
 * is per-unit, not a blanket dialog, so it never reads as nagging.
 */
const PROTECTED_UNITS = new Set([
  "argus-agent.service",
  "containerd.service",
  "dbus.service",
  "docker.service",
  "NetworkManager.service",
  "ssh.service",
  "sshd.service",
  "systemd-journald.service",
  "systemd-logind.service",
  "systemd-networkd.service",
  "systemd-resolved.service",
]);

export function isProtectedUnit(name: string): boolean {
  return PROTECTED_UNITS.has(name);
}

/**
 * The rows to render: optionally narrowed to failures, optionally filtered by a
 * case-insensitive substring of the name or description, then sorted
 * failed → active → other and alphabetically within each group.
 */
export function visibleUnits(units: Unit[], filter: string, failedOnly: boolean): Unit[] {
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
