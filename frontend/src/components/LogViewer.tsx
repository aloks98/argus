// The shared log view. Drives LazyLog in controlled `text` mode: we own the
// EventSource and the line buffer, because the library's `eventsource` mode is
// append-only and loading older pages needs to PREPEND to the buffer.
// Behaviour of the live tail is unchanged — stream, follow, search, select.
import { useEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import { LazyLog } from "@melloware/react-logviewer";
import { fetchLogPage, logStreamUrl, ALL_LOGS } from "../api";
import type { LogFilters } from "../api";
import type { LogLine } from "../lib/logs";
import { formatLogLine, levelTone, parseLogParts, parseNdjsonBatch } from "../lib/logs";
import type { Tone } from "../lib/status";

const LEVEL_TEXT: Record<Tone, string> = {
  ok: "text-[var(--ok-text)]",
  warn: "text-[var(--warn-text)]",
  fail: "text-[var(--fail-text)]",
  idle: "text-foreground",
};

/** Bound the buffer so a long session can't grow unbounded. */
const MAX_LINES = 50_000;

export default function LogViewer({
  machineId,
  source,
  filters = ALL_LOGS,
  height,
}: {
  machineId: string;
  source: string;
  filters?: LogFilters;
  height?: number;
}) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [reachedStart, setReachedStart] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  // `follow` pauses when the operator scrolls up and resumes at the bottom.
  const [following, setFollowing] = useState(true);
  // Target line to hold the viewport after a prepend (LazyLog scrollToLine).
  const [anchorLine, setAnchorLine] = useState<number | undefined>(undefined);
  const loadingRef = useRef(false);
  // Last scrollTop LazyLog reported, so the wheel handler knows we're at the top.
  const lastScrollTopRef = useRef(0);
  // Pending rAF handles for the deferred post-prepend anchor (cancelled on unmount).
  const anchorRaf1 = useRef(0);
  const anchorRaf2 = useRef(0);
  // Bumped every time the reset effect below rebuilds the stream (source or
  // filter change). `loadOlder` captures the value before its `await` and
  // discards a response that resolves after the generation has moved on, so a
  // stale in-flight page can never prepend into a freshly-reset, differently
  // filtered buffer.
  const runIdRef = useRef(0);
  // The cutoff this tail resolved, learned from the stream's `meta` frame.
  // A ref, not state: it must be readable by `loadOlder` without making the
  // stream effect depend on it and tear the EventSource down.
  const sinceMsRef = useRef<number | undefined>(undefined);

  const isJournal = source.startsWith("journal:");

  // Cancel any in-flight anchor frames if we unmount mid-load.
  useEffect(
    () => () => {
      cancelAnimationFrame(anchorRaf1.current);
      cancelAnimationFrame(anchorRaf2.current);
    },
    [],
  );

  // Own the EventSource so the buffer is ours to append to and to prepend
  // older pages into. Re-created when machine, source, or filters change.
  useEffect(() => {
    // A new generation: any `loadOlder` call still in flight from the
    // previous machine/source/filters must not touch the buffer being reset
    // here once its response arrives.
    runIdRef.current += 1;
    setLines([]);
    setReachedStart(false);
    setFollowing(true);
    setAnchorLine(undefined);
    const es = new EventSource(logStreamUrl(machineId, source, filters));
    sinceMsRef.current = undefined;
    // Named events do NOT reach `onmessage`, so this can't collide with the
    // NDJSON log frames. The server re-announces a fresh cutoff on every
    // reconnect (routine — it ends the SSE stream whenever an agent session
    // tears down), but the line buffer is NOT cleared on a reconnect, only on
    // machine/source/filter change — so adopting a later cutoff here would pair
    // it with an older buffer: `loadOlder` would send a cutoff newer than the
    // anchor already in view, `finalize_page` would truncate the page to
    // nothing, and the viewer would report "beginning of window" while still
    // showing an older span. So only the FIRST announced cutoff for this
    // buffer's lifetime is adopted; later reconnects' cutoffs are discarded.
    es.addEventListener("meta", (e) => {
      if (sinceMsRef.current !== undefined) return;
      try {
        const m = JSON.parse((e as MessageEvent).data) as { since_ms?: number };
        sinceMsRef.current = typeof m.since_ms === "number" ? m.since_ms : undefined;
      } catch {
        // A malformed meta frame just means we page without an explicit cutoff.
      }
    });
    es.onmessage = (e) => {
      const batch = parseNdjsonBatch(e.data);
      if (batch.length === 0) return;
      setLines((prev) => {
        const next = prev.concat(batch);
        return next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
      });
    };
    // The browser's EventSource auto-reconnects; nothing to do on error.
    return () => es.close();
    // Depends on `filters.priority`/`filters.window`, not `filters` itself,
    // deliberately: `useLogFilters` derives a fresh `LogFilters` object from
    // the URL params on every render (see lib/logFilters.ts), so its
    // reference changes on every unrelated re-render (e.g. this page's 10s
    // metrics poll). Depending on the object would tear down and reopen the
    // EventSource on every one of those, not just on an actual filter change.
    // oxlint-disable-next-line react-hooks/exhaustive-deps
  }, [machineId, source, filters.priority, filters.window]);

  const text = useMemo(() => lines.map(formatLogLine).join("\n"), [lines]);

  async function loadOlder() {
    if (loadingRef.current || reachedStart || !isJournal) return;
    // The oldest cursor we hold — the first line with one (markers have none).
    const oldest = lines.find((l) => l.cursor)?.cursor;
    if (oldest === undefined || oldest === null) return;
    loadingRef.current = true;
    setLoadingOlder(true);
    // react-logviewer only re-scrolls when scrollToLine's VALUE changes across
    // renders. Pages are a fixed 500 lines, so the target would be the same
    // number two loads in a row and the second load's anchor would silently
    // no-op. Clear it first so every load transitions through a distinct
    // (undefined) value before landing on the real target below.
    setAnchorLine(undefined);
    // Captured before the `await`: if a filter/source change bumps this while
    // the request is in flight, the reset effect has already cleared `lines`
    // and reopened the stream, so this response is stale and must be dropped.
    const runId = runIdRef.current;
    try {
      const page = await fetchLogPage(machineId, source, oldest, filters, sinceMsRef.current);
      if (runId !== runIdRef.current) return;
      if (page.lines.length > 0) {
        const seam = page.lines.length + 1;
        setLines((prev) => {
          const merged = page.lines.concat(prev);
          return merged.length > MAX_LINES ? merged.slice(0, MAX_LINES) : merged;
        });
        // The line that was at the top is now shifted down by page.lines.length;
        // scroll to it (1-based) so the viewport does not jump. Defer this two
        // frames: LazyLog re-parses `text` on a follow-up render and virtua then
        // lays out the new rows, so scrolling in the same commit as the prepend
        // races that and leaves the viewport pinned at the top (offset 0), where
        // no scroll events fire and the next load can't be triggered.
        cancelAnimationFrame(anchorRaf1.current);
        cancelAnimationFrame(anchorRaf2.current);
        anchorRaf1.current = requestAnimationFrame(() => {
          anchorRaf2.current = requestAnimationFrame(() => setAnchorLine(seam));
        });
      }
      if (page.reached_start) setReachedStart(true);
    } catch {
      // Leave the buffer intact; a transient failure just means no older lines
      // loaded this time.
    } finally {
      loadingRef.current = false;
      setLoadingOlder(false);
    }
  }

  function onScroll({
    scrollTop,
    scrollHeight,
    clientHeight,
  }: {
    scrollTop: number;
    scrollHeight: number;
    clientHeight: number;
  }) {
    lastScrollTopRef.current = scrollTop;
    // Near the top: load older (journal only).
    if (scrollTop <= 40) void loadOlder();
    // Follow only while at the bottom.
    const atBottom = scrollHeight - scrollTop - clientHeight <= 40;
    setFollowing(atBottom);
  }

  function onWheel(e: WheelEvent) {
    // virtua only emits onScroll when the offset actually changes, so once the
    // viewport is parked at the very top (offset 0) an upward scroll produces no
    // event and the onScroll trigger above can never re-fire. Catch the upward
    // wheel here so "load older" keeps working when parked at the top.
    if (e.deltaY < 0 && lastScrollTopRef.current <= 40) void loadOlder();
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {isJournal && (
        <div className="pb-1 text-center font-mono text-[10px] uppercase tracking-widest text-muted-foreground">
          {reachedStart
            ? filters.window === "all"
              ? "— beginning of journal —"
              : "— beginning of window —"
            : loadingOlder
              ? "loading older…"
              : "scroll up to load older"}
        </div>
      )}
      {/* `dark` is deliberate and not a theme bug: LazyLog's own surface is
          always dark, but the row colours below come from theme tokens, so in
          light mode `--foreground` resolves to near-black and the log text
          renders dark-on-dark. Scoping a dark token context to just this
          subtree keeps the rows readable in both themes. It is applied here
          rather than on the component root so the "scroll up to load older"
          status line above still follows the page theme. */}
      <div className="dark min-h-0 flex-1" onWheel={onWheel}>
        <LazyLog
          // Remount on a source switch so an in-progress search term and scroll
          // position don't carry across from the previous unit's logs.
          key={source}
          text={text}
          follow={following}
          onScroll={onScroll}
          scrollToLine={anchorLine}
          selectableLines
          enableSearch
          enableSearchNavigation
          caseInsensitive
          height={height}
          formatPart={(part: string) => {
            const { ts, level, ident, msg } = parseLogParts(part);
            return (
              <span className="font-mono text-xs">
                <span className="mr-2 inline-block text-muted-foreground tabular-nums">{ts}</span>
                <span className="mr-2 inline-block min-w-[12ch] text-muted-foreground">{ident}</span>
                <span className={LEVEL_TEXT[levelTone(level)]}>{msg}</span>
              </span>
            );
          }}
        />
      </div>
    </div>
  );
}
