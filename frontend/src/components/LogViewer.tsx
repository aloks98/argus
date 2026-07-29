// The shared log view. Drives LazyLog in controlled `text` mode: we own the
// EventSource and the line buffer, because the library's `eventsource` mode
// is append-only and loading older pages needs to PREPEND to the buffer.
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
  // Bumped whenever the reset effect below rebuilds the stream (source or
  // filter change). `loadOlder` captures the value before its `await` and
  // discards a stale response, so it can never prepend into a freshly-reset,
  // differently filtered buffer.
  const runIdRef = useRef(0);
  // The cutoff this tail resolved, learned from the stream's `meta` frame.
  // A ref, not state: it must be readable by `loadOlder` without making the
  // stream effect depend on it and tear the EventSource down.
  const sinceMsRef = useRef<number | undefined>(undefined);

  const isJournal = source.startsWith("journal:");

  useEffect(
    () => () => {
      cancelAnimationFrame(anchorRaf1.current);
      cancelAnimationFrame(anchorRaf2.current);
    },
    [],
  );

  useEffect(() => {
    runIdRef.current += 1;
    setLines([]);
    setReachedStart(false);
    setFollowing(true);
    setAnchorLine(undefined);
    const es = new EventSource(logStreamUrl(machineId, source, filters));
    sinceMsRef.current = undefined;
    // Named events don't reach `onmessage`. The server re-announces a cutoff
    // on every reconnect (an agent session tear-down ends the SSE stream —
    // routine), but the buffer only clears on machine/source/filter change.
    // Adopting a later cutoff would pair it with an older buffer and desync
    // `loadOlder`'s paging math — so only the FIRST cutoff per buffer
    // lifetime is adopted.
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
    // Depends on filters.priority/window, not `filters` itself: useLogFilters
    // derives a fresh object from URL params every render (lib/logFilters.ts),
    // so depending on the object would tear the EventSource down on every
    // unrelated re-render (e.g. the 10s metrics poll), not just a real change.
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
    // react-logviewer only re-scrolls when scrollToLine's VALUE changes.
    // Pages are a fixed 500 lines, so two loads in a row would target the
    // same number and the second anchor would silently no-op — clear it
    // first so every load passes through a distinct (undefined) value.
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
        // The old top line is now shifted down by page.lines.length; scroll
        // to it (1-based) so the viewport doesn't jump. Deferred two frames
        // — LazyLog re-parses `text` then virtua lays out the new rows, so
        // scrolling in the same commit races that and pins the viewport at
        // offset 0, where the next load can never trigger.
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
    const atBottom = scrollHeight - scrollTop - clientHeight <= 40;
    setFollowing(atBottom);
  }

  function onWheel(e: WheelEvent) {
    // virtua only emits onScroll when the offset actually changes, so once
    // parked at the very top (offset 0) an upward scroll fires no event and
    // onScroll's trigger above can't re-fire. Catch it here instead.
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
      {/* `dark` here is deliberate, not a theme bug: LazyLog's surface is
          always dark, but the row colours come from theme tokens, so in
          light mode `--foreground` resolves near-black and text renders
          dark-on-dark. Scoped to this subtree (not the component root) so
          the status line above still follows the page theme. */}
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
                <span className="mr-2 inline-block min-w-[12ch] text-muted-foreground">
                  {ident}
                </span>
                <span className={LEVEL_TEXT[levelTone(level)]}>{msg}</span>
              </span>
            );
          }}
        />
      </div>
    </div>
  );
}
