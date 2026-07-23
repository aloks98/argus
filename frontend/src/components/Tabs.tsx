import { useRef } from "react";
import { cn } from "../lib/cn";

export type TabKey = string;

/**
 * Tab strip implementing the ARIA tabs pattern. Panels must be rendered by the
 * caller with `role="tabpanel"`, `id={`panel-${key}`}` and
 * `aria-labelledby={`tab-${key}`}` so the ids here line up.
 */
export default function Tabs({
  tabs,
  active,
  onChange,
}: {
  tabs: { key: TabKey; label: string }[];
  active: TabKey;
  onChange: (key: TabKey) => void;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);

  function onKeyDown(e: React.KeyboardEvent<HTMLDivElement>) {
    if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
    e.preventDefault();
    const i = tabs.findIndex((t) => t.key === active);
    if (i === -1) return;
    const next = e.key === "ArrowRight" ? (i + 1) % tabs.length : (i - 1 + tabs.length) % tabs.length;
    onChange(tabs[next].key);
    refs.current[next]?.focus();
  }

  return (
    <div role="tablist" onKeyDown={onKeyDown} className="flex border-y-2 border-border">
      {tabs.map((t, i) => (
        <button
          key={t.key}
          ref={(el) => {
            refs.current[i] = el;
          }}
          role="tab"
          id={`tab-${t.key}`}
          aria-controls={`panel-${t.key}`}
          aria-selected={t.key === active}
          tabIndex={t.key === active ? 0 : -1}
          onClick={() => onChange(t.key)}
          className={cn(
            "px-3 py-2 text-[10px] uppercase tracking-widest",
            t.key === active
              ? "bg-primary font-bold text-primary-foreground"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {t.label}
        </button>
      ))}
    </div>
  );
}
