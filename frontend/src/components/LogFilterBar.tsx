// Priority + time-window controls, shared by the Logs tab and the per-unit
// dialog so there is one control and one code path for both journal surfaces.
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@e412/rnui-react";
import type { LogFilters, LogWindow } from "../api";

/**
 * Syslog severities. Lower is MORE severe: `-p 4` returns 4,3,2,1,0. All eight
 * accepted values (0-7) need an entry here, or a URL/filter carrying a value
 * this list doesn't list (e.g. `?priority=2`) renders a blank Select trigger
 * while the filter is silently still active. `0` is the odd one out: it means
 * UNSET (no `-p` at all), not "emerg only" — nobody wants emerg-only — so it
 * keeps its "all severities" no-filter label rather than gaining a real
 * `emerg` entry.
 */
const PRIORITIES: { value: string; label: string }[] = [
  { value: "0", label: "all severities" },
  { value: "1", label: "alert and worse" },
  { value: "2", label: "crit and worse" },
  { value: "3", label: "err and worse" },
  { value: "4", label: "warning and worse" },
  { value: "5", label: "notice and worse" },
  { value: "6", label: "info and worse" },
  { value: "7", label: "debug and worse" },
];

const WINDOWS: { value: LogWindow; label: string }[] = [
  { value: "boot", label: "current boot" },
  { value: "1h", label: "last hour" },
  { value: "24h", label: "last 24h" },
  { value: "all", label: "all history" },
];

export default function LogFilterBar({
  value,
  onChange,
}: {
  value: LogFilters;
  onChange: (next: LogFilters) => void;
}) {
  return (
    <div className="flex items-center gap-2 pb-2">
      {/* `items` is what makes the closed trigger show the LABEL. Without it
          base-ui's SelectValue renders the raw value, so the trigger read "5"
          instead of "notice and worse". A `{value, label}` array is used
          automatically. */}
      <Select
        items={PRIORITIES}
        value={String(value.priority)}
        onValueChange={(v: string | null) =>
          onChange({ ...value, priority: Number(v ?? "0") })
        }
      >
        <SelectTrigger size="sm" className="w-48 font-mono text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {PRIORITIES.map((p) => (
            <SelectItem key={p.value} value={p.value}>
              {p.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>

      <Select
        items={WINDOWS}
        value={value.window}
        onValueChange={(v: string | null) =>
          onChange({ ...value, window: (v ?? "all") as LogWindow })
        }
      >
        <SelectTrigger size="sm" className="w-40 font-mono text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {WINDOWS.map((w) => (
            <SelectItem key={w.value} value={w.value}>
              {w.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
