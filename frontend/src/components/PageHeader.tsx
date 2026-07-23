import { cn } from "../lib/cn";

/** Uppercase display title. */
export default function PageHeader({
  title,
  meta,
  className,
}: {
  title: React.ReactNode;
  meta?: React.ReactNode;
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
    </div>
  );
}
