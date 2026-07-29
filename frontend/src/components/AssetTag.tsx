import { Badge } from "@e412/rnui-react";
import type { Tone } from "../lib/status";
import { cn } from "../lib/cn";

/**
 * A hostname (or container name) rendered as a solid stencilled asset tag,
 * fill = status.
 *
 * Text is forced black on every fill via `className`, overriding `Badge`'s
 * own white-text `success`/`warning`/`destructive` variants: black clears
 * WCAG AA (4.5:1) on all four fills, where white on `fail` would not
 * (3.85:1 vs black's 5.46:1).
 *
 * `idle` also forces its own theme-invariant `--idle` fill rather than a
 * variant background, since `--muted-foreground` differs per theme and
 * couldn't pass contrast in both.
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
  const variant =
    tone === "ok"
      ? "success"
      : tone === "warn"
        ? "warning"
        : tone === "fail"
          ? "destructive"
          : "secondary";

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
