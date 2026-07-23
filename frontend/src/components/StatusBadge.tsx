import { statusTextVariants } from "../lib/status";
import type { Tone } from "../lib/status";
import { cn } from "../lib/cn";

/**
 * The text half of a status. Always rendered alongside any colour cue so status
 * is never conveyed by colour alone.
 */
export default function StatusBadge({
  tone,
  label,
  className,
}: {
  tone: Tone;
  label: string;
  className?: string;
}) {
  return <span className={cn(statusTextVariants({ tone }), className)}>{label}</span>;
}
