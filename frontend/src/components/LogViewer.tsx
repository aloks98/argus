// The shared log view, used by both the drawer and the full-page route.
//
// Built on LazyLog, which owns the EventSource, virtualization, search and
// follow. We keep only the rendering: `formatMessage` maps one NDJSON payload
// to a display line, and `formatPart` turns that line back into our own JSX so
// severity uses the design tokens rather than ANSI escapes.
//
// Theming is deliberately out of scope for this slice — LazyLog's dark-terminal
// default ships as-is; matching it to the palette (and to light mode) is a
// follow-up.
import { LazyLog } from "@melloware/react-logviewer";
import { logStreamUrl } from "../api";
import { formatLogMessage, levelTone, parseLogParts } from "../lib/logs";
import type { Tone } from "../lib/status";

// Severity → *text colour only*. Deliberately NOT `statusTextVariants`, which
// also forces `uppercase tracking-wider` — right for a status badge, wrong for
// log lines, which must read verbatim. A normal (idle) line is full-contrast
// foreground, not muted, so INFO logs stay readable; only warn/error get colour.
const LEVEL_TEXT: Record<Tone, string> = {
  ok: "text-[var(--ok-text)]",
  warn: "text-[var(--warn-text)]",
  fail: "text-[var(--fail-text)]",
  idle: "text-foreground",
};

export default function LogViewer({
  machineId,
  source,
  height,
}: {
  machineId: string;
  source: string;
  height?: number;
}) {
  return (
    <LazyLog
      key={`${machineId}:${source}`}
      url={logStreamUrl(machineId, source)}
      eventsource
      follow
      // Off by default in the library; without it the log text cannot be
      // selected or copied — the first thing anyone wants to do with a log.
      selectableLines
      enableSearch
      enableSearchNavigation
      caseInsensitive
      height={height}
      eventsourceOptions={{
        reconnect: true,
        formatMessage: (message: unknown) => formatLogMessage(message),
      }}
      formatPart={(text: string) => {
        const { ts, level, ident, msg } = parseLogParts(text);
        // Fixed-width time and identifier columns so the message text lines up
        // down the page regardless of identifier length (`systemd` vs `sh` vs
        // `sshd-session`). Monospace + `inline-block` min-widths; a rare long
        // identifier extends its own cell rather than breaking every other row.
        return (
          <span className="font-mono text-xs">
            <span className="mr-2 inline-block text-muted-foreground tabular-nums">
              {ts}
            </span>
            <span className="mr-2 inline-block min-w-[12ch] text-muted-foreground">
              {ident}
            </span>
            <span className={LEVEL_TEXT[levelTone(level)]}>{msg}</span>
          </span>
        );
      }}
    />
  );
}
