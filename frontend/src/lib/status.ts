import { cva } from "class-variance-authority";

/** The semantic tones. Colour never carries status alone — always paired with text. */
export type Tone = "ok" | "warn" | "fail" | "idle";

/** machines.status from the control plane. */
export function machineTone(status: string): Tone {
  switch (status) {
    case "online":
      return "ok";
    case "pending":
      return "warn";
    case "offline":
      return "fail";
    default:
      return "idle";
  }
}

/** Docker container state (running|exited|paused|restarting|created|dead|removing). */
export function containerTone(state: string): Tone {
  switch (state) {
    case "running":
      return "ok";
    case "restarting":
    case "paused":
      return "warn";
    case "dead":
      return "fail";
    case "exited":
    case "created":
    case "removing":
      return "idle";
    default:
      return "idle";
  }
}

/** systemd ActiveState (active|failed|inactive|activating|deactivating|reloading). */
export function unitTone(activeState: string): Tone {
  switch (activeState) {
    case "active":
      return "ok";
    case "activating":
    case "deactivating":
    case "reloading":
      return "warn";
    case "failed":
      return "fail";
    default:
      return "idle";
  }
}

/**
 * Enrollment-token lifecycle, derived client-side from its four contributing
 * fields (there is no `state` column on the wire). Order matters: a revoked
 * token reads "revoked" even if it's also past its expiry or used up, and a
 * used-up token reads that way even if it also happens to be expired.
 */
export type TokenState = "revoked" | "used up" | "expired" | "active";

export function tokenState(t: {
  revoked: boolean;
  max_uses: number | null;
  uses: number;
  expires_at: string | null;
}): TokenState {
  if (t.revoked) return "revoked";
  if (t.max_uses !== null && t.uses >= t.max_uses) return "used up";
  if (t.expires_at !== null && Date.parse(t.expires_at) < Date.now()) return "expired";
  return "active";
}

/** fail/warn/warn/ok, per the design. */
export function tokenTone(state: TokenState): Tone {
  switch (state) {
    case "revoked":
      return "fail";
    case "used up":
    case "expired":
      return "warn";
    case "active":
      return "ok";
  }
}

/** Text-only status, using the theme-aware *-text variants (readable on white). */
export const statusTextVariants = cva("font-mono text-xs uppercase tracking-wider", {
  variants: {
    tone: {
      ok: "text-[var(--ok-text)]",
      warn: "text-[var(--warn-text)]",
      fail: "text-[var(--fail-text)]",
      idle: "text-[var(--muted-foreground)]",
    },
  },
  defaultVariants: { tone: "idle" },
});
