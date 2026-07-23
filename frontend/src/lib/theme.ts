import { useSyncExternalStore } from "react";

/**
 * A single module-level MutationObserver, shared by every chart, that bumps a
 * version counter whenever the theme class on <html> changes. Components read
 * the version so they re-render and re-resolve CSS token colours on a toggle.
 * One observer for all N sparklines, not one per instance.
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
