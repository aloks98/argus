import type { ReactNode } from "react";

export type SpecItem = { label: string; value: ReactNode };

export default function SpecStrip({ items }: { items: SpecItem[] }) {
  return (
    <div className="flex flex-wrap border border-border">
      {items.map((item, i) => (
        <div key={i} className="border-r border-border px-3 py-1.5 last:border-r-0">
          <div className="mb-0.5 text-[9px] uppercase tracking-[0.16em] text-muted-foreground">
            {item.label}
          </div>
          <div className="font-mono text-[11px]">{item.value}</div>
        </div>
      ))}
    </div>
  );
}
