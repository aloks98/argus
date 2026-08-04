// Presentation logic for the audit page: the static event map (user-requested
// light humanization) and the result → tone mapping. Pure functions, no I/O.
import type { Tone } from "./status";

/** Filterable action namespaces — mirror of the server's AUDIT_CATEGORIES. */
export const AUDIT_CATEGORIES = [
  "agent",
  "auth",
  "container",
  "unit",
  "logs",
  "terminal",
  "enroll_token",
  "machine",
  "local_admin",
] as const;

/**
 * Short human phrase per audit action. An unknown action falls back to the
 * raw string, so a server-side action added before this map learns it
 * degrades to machine-truth instead of blanking.
 */
const EVENT_LABELS: Record<string, string> = {
  "agent.enroll": "agent enrolled",
  "agent.online": "agent connected",
  "auth.login": "signed in",
  "auth.logout": "signed out",
  "auth.denied": "sign-in denied",
  "container.start": "started container",
  "container.stop": "stopped container",
  "container.restart": "restarted container",
  "unit.start": "started unit",
  "unit.stop": "stopped unit",
  "unit.restart": "restarted unit",
  "logs.open": "opened log tail",
  "logs.page": "read older logs",
  "terminal.open": "opened terminal",
  "machine.update": "edited machine identity",
  "enroll_token.create": "minted enrollment token",
  "enroll_token.revoke": "revoked enrollment token",
  "local_admin.reset": "reset local admin password",
  "local_admin.rotate": "rotated local admin password",
};

export function eventLabel(action: string): string {
  return EVENT_LABELS[action] ?? action;
}

export function resultTone(result: string | null): Tone {
  switch (result) {
    case "ok":
      return "ok";
    case "denied":
      return "warn";
    case "error":
      return "fail";
    default:
      return "idle";
  }
}
