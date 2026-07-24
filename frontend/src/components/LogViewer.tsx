// The shared log view. Drives LazyLog in controlled `text` mode: we own the
// EventSource and the line buffer, because the library's `eventsource` mode is
// append-only and Task 7 needs to PREPEND older pages. Behaviour of the live
// tail is unchanged — stream, follow, search, select.
import { useEffect, useMemo, useRef, useState, type WheelEvent } from "react";
import { LazyLog } from "@melloware/react-logviewer";
import { fetchLogPage, logStreamUrl } from "../api";
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
  height,
}: {
  machineId: string;
  source: string;
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

  const isJournal = source.startsWith("journal:");

  // Cancel any in-flight anchor frames if we unmount mid-load.
  useEffect(
    () => () => {
      cancelAnimationFrame(anchorRaf1.current);
      cancelAnimationFrame(anchorRaf2.current);
    },
    [],
  );

  // Own the EventSource so the buffer is ours to append to (and, in Task 7,
  // prepend to). Re-created when machine or source changes.
  useEffect(() => {
    setLines([]);
    setReachedStart(false);
    setFollowing(true);
    setAnchorLine(undefined);
    const es = new EventSource(logStreamUrl(machineId, source));
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
  }, [machineId, source]);

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
    try {
      const page = await fetchLogPage(machineId, source, oldest);
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
            ? "— beginning of journal —"
            : loadingOlder
              ? "loading older…"
              : "scroll up to load older"}
        </div>
      )}
      <div className="min-h-0 flex-1" onWheel={onWheel}>
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
