import { Badge } from "@e412/rnui-react";
import type { Tone } from "../lib/status";
import { cn } from "../lib/cn";

/**
 * A hostname (or container name) rendered as a solid stencilled asset tag whose
 * fill is its status — the design's signature element.
 *
 * Built on rnui's `Badge`. Text colour is black on every fill, verified against
 * WCAG AA 4.5:1: ok #00E676 12.58:1 · warn #FF6D00 7.44:1 · fail #FF1744 5.46:1 ·
 * idle #8A8A8A 6.08:1. (White on fail would be only 3.85:1 — saturated reds read
 * better with black.) `Badge`'s own `success`/`warning`/`destructive` variants
 * render white text (`text-white`), so `text-black` is forced via `className` on
 * every tone to hold that contrast rule regardless of the base variant's default.
 * `idle` additionally forces its theme-invariant `--idle` fill rather than relying
 * on any variant's built-in background, since `--muted-foreground` differs per
 * theme and could not pass contrast in both.
 */
export default function AssetTag({
  tone,
  children,
  className,
}: {
  tone: Tone;
  children: React.ReactNode;
  className?: string;
}) {
  const variant = tone === "ok" ? "success" : tone === "warn" ? "warning" : tone === "fail" ? "destructive" : "secondary";

  return (
    <Badge
      variant={variant}
      className={cn(
        "font-mono text-xs font-bold uppercase tracking-wider text-black",
        tone === "idle" && "bg-[var(--idle)]",
        className,
      )}
    >
      {children}
    </Badge>
  );
}
