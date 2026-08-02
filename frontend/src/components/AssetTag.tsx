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
/**
 * A unit/container name whose loudness tracks its state: the filled tag is
 * reserved for exceptions (`warn`/`fail`) so a long table of healthy rows
 * reads as quiet mono text with the failures shouting — not a wall of green
 * blocks. `idle` dims to muted. On md+ the adjacent status column carries
 * the state as TEXT; on phones (column hidden) the split still separates
 * exception from normal by shape — tag vs plain — not by colour alone.
 */
export function StatusName({
  tone,
  name,
  className,
}: {
  tone: Tone;
  name: string;
  className?: string;
}) {
  // `pointer-coarse:` lifts truncation on touch devices: the full name lives
  // in the cell's `title` there too, but no hover means no way to read it —
  // wrapping (rare: only ~90-char escaped device names) beats hiding it.
  if (tone === "warn" || tone === "fail") {
    return (
      <AssetTag tone={tone} className={cn("max-w-[16ch] md:max-w-[30ch]", className)}>
        <span className="min-w-0 truncate pointer-coarse:whitespace-normal pointer-coarse:break-all">
          {name}
        </span>
      </AssetTag>
    );
  }
  return (
    <span
      className={cn(
        "block max-w-[16ch] truncate font-mono text-xs md:max-w-[30ch]",
        "pointer-coarse:whitespace-normal pointer-coarse:break-all",
        tone === "idle" && "text-muted-foreground",
        className,
      )}
    >
      {name}
    </span>
  );
}

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
