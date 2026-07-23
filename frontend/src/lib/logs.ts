// The two halves of the LazyLog seam, kept pure and out of the component.
//
// `formatMessage` must return a string and `formatPart` receives only that
// string — never the original object — so severity has to survive as text.
// These two functions are the encoder and decoder of that hop; they must agree
// on the prefix, which is why they live together.
import type { Tone } from "./status";

export type LogLine = {
  ts: number;
  level: number | null;
  ident: string | null;
  msg: string;
  marker?: boolean;
};

/** Width of the encoded level field, so the decoder can split by index. */
const LEVEL_WIDTH = 1;

/**
 * NDJSON batch -> the display lines LazyLog stores.
 *
 * The agent batches many NDJSON records into one `LogChunk`; after the SSE
 * hop, `EventSource` rejoins them into a single `message` event whose `.data`
 * is every record joined by `\n`, and `@melloware/react-logviewer` calls
 * `formatMessage` exactly ONCE with that whole multi-line blob. So this must
 * split the blob back into records, format each to exactly one display line,
 * and rejoin with `\n` — LazyLog then re-splits our return value into one
 * visual row per record and calls `formatPart` on each.
 */
export function formatLogMessage(raw: unknown): string {
  return String(raw)
    .split("\n")
    .filter((record) => record.length > 0)
    .map(formatOneRecord)
    .join("\n");
}

/**
 * One NDJSON record -> the single display line LazyLog stores.
 * Layout: `<level><ts-iso> <ident>\t<msg>` where level is one char (`0`-`7`,
 * or `-` when the source has no severity).
 */
function formatOneRecord(record: string): string {
  let line: LogLine;
  try {
    line = JSON.parse(record) as LogLine;
  } catch {
    return `-       \t${record}`;
  }
  const level = line.level === null || line.level === undefined ? "-" : String(line.level);
  const time = new Date(line.ts).toISOString().slice(11, 19);
  const ident = line.ident ?? "";
  // Collapse any newline INSIDE one record's message: a multi-line MESSAGE
  // (e.g. a stack trace) is one NDJSON record and must stay one display line,
  // or its continuation rows would lose the level/ts/ident prefix and misparse
  // in formatPart.
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
