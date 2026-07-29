import { useSyncExternalStore } from "react";

/**
 * One module-level MutationObserver, shared by every chart (not one per
 * instance), bumping a version counter whenever <html>'s theme class
 * changes — components read it to re-render and re-resolve CSS token colors.
 */
let version = 0;
const listeners = new Set<() => void>();
let observer: MutationObserver | null = null;

function ensureObserver(): void {
  if (observer !== null || typeof document === "undefined") return;
  observer = new MutationObserver(() => {
    version += 1;
    listeners.forEach((l) => l());
  });
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["class"],
  });
}

function subscribe(cb: () => void): () => void {
  ensureObserver();
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

/** Re-renders the caller whenever the <html> theme class flips. */
export function useThemeVersion(): number {
  return useSyncExternalStore(
    subscribe,
    () => version,
    () => 0,
  );
}
