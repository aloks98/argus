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
