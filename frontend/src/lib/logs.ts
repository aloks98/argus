// The two halves of the LazyLog seam, kept pure and out of the component.
//
// LazyLog's controlled `text` mode stores one string per line, never the
// original object, so severity must survive as text: `formatLogLine` encodes
// a `LogLine` into that string, `parseLogParts` decodes it back — they must
// agree on the prefix, which is why they live together.
import type { Tone } from "./status";

export type LogLine = {
  ts: number;
  level: number | null;
  ident: string | null;
  msg: string;
  marker?: boolean;
  cursor?: string | null;
};

/** Width of the encoded level field, so the decoder can split by index. */
const LEVEL_WIDTH = 1;

/** Parse one SSE/page NDJSON blob into structured lines, dropping blanks and
 *  anything unparseable (a malformed record must not break the batch). */
export function parseNdjsonBatch(blob: string): LogLine[] {
  const out: LogLine[] = [];
  for (const record of String(blob).split("\n")) {
    if (record.length === 0) continue;
    try {
      out.push(JSON.parse(record) as LogLine);
    } catch {
      out.push({ ts: 0, level: null, ident: null, msg: record });
    }
  }
  return out;
}

/** A LogLine -> the single display line LazyLog stores in `text` mode.
 *  Layout: `<level><ts-iso> <ident>\t<msg>`; level is one char (`0`-`7` or `-`). */
export function formatLogLine(line: LogLine): string {
  const level = line.level === null || line.level === undefined ? "-" : String(line.level);
  const time = new Date(line.ts).toISOString().slice(11, 19);
  const ident = line.ident ?? "";
  // A multi-line MESSAGE is one record and must stay one display row.
  const msg = line.msg.replace(/\r?\n/g, " ⏎ ");
  return `${level}${time} ${ident}\t${msg}`;
}

/** The inverse: split a display line back into its parts for rendering. */
export function parseLogParts(text: string): {
  ts: string;
  level: number | null;
  ident: string;
  msg: string;
} {
  const levelChar = text.slice(0, LEVEL_WIDTH);
  const level = levelChar === "-" ? null : Number(levelChar);
  const rest = text.slice(LEVEL_WIDTH);
  const tab = rest.indexOf("\t");
  if (tab === -1) return { ts: "", level, ident: "", msg: rest };
  const head = rest.slice(0, tab);
  const msg = rest.slice(tab + 1);
  const space = head.indexOf(" ");
  const ts = space === -1 ? head : head.slice(0, space);
  const ident = space === -1 ? "" : head.slice(space + 1);
  return { ts, level, ident, msg };
}

/**
 * syslog priority -> a design-system tone. 0-3 are emerg..err, 4 is warning,
 * 5-7 are notice..debug. `null` (docker, which has no severity) is neutral.
 */
export function levelTone(level: number | null): Tone {
  if (level === null) return "idle";
  if (level <= 3) return "fail";
  if (level === 4) return "warn";
  return "idle";
}
