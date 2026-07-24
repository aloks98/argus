// URL-backed log filter state, shared by both log surfaces.
//
// The Logs tab and the per-unit dialog read and write the same
// `?priority=` / `?window=` params, so the read/write pair lives here instead of
// being hand-rolled in each component — they had drifted into two byte-identical
// copies of the same six lines.
import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";
import { filtersFromParams } from "../api";
import type { LogFilters } from "../api";

/**
 * `[filters, setFilters]` backed by the query string, so a filtered view is
 * linkable and survives a reload.
 *
 * `fallback` is the per-surface default and the two differ deliberately: the
 * Logs tab defaults to the current boot, the per-unit dialog to all history.
 * Defaulting per-unit to the current boot would be a regression — `journalctl
 * -b` composes with `--cursor`, so it would silently cap scroll-back at the last
 * reboot on a view that can otherwise page back indefinitely.
 */
export function useLogFilters(
  fallback: LogFilters,
): [LogFilters, (next: LogFilters) => void] {
  const [searchParams, setSearchParams] = useSearchParams();
  const filters = filtersFromParams(searchParams, fallback);

  const setFilters = useCallback(
    (next: LogFilters) => {
      // Functional form on purpose: react-router hands back the params as they
      // are at apply time, so this never writes a copy captured at render time
      // and can't clobber a concurrent `?tab=` change on the same page.
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          p.set("window", next.window);
          // `priority: 0` means "no filter" — leave the param off entirely
          // rather than writing a value that only means the default.
          if (next.priority > 0) p.set("priority", String(next.priority));
          else p.delete("priority");
          return p;
        },
        { replace: true },
      );
    },
    [setSearchParams],
  );

  return [filters, setFilters];
}
