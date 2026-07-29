import { cn } from "../lib/cn";

export default function PageHeader({
  title,
  meta,
  actions,
  className,
}: {
  title: React.ReactNode;
  meta?: React.ReactNode;
  /** Right-aligned controls (e.g. a page-level primary action button) —
   * the outer flex row is already `justify-between`, so this just needs to
   * exist as a second child to land far right. */
  actions?: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex flex-wrap items-center justify-between gap-2 py-3", className)}>
      <div>
        <h1 className="font-display text-xl uppercase tracking-tight">{title}</h1>
        {meta !== undefined && (
          <p className="mt-1 font-mono text-xs text-muted-foreground">{meta}</p>
        )}
      </div>
      {actions !== undefined && <div className="flex items-center gap-2">{actions}</div>}
    </div>
  );
}
