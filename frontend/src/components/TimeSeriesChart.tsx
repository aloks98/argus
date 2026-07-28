import { useEffect, useMemo, useRef, useState } from "react";
import UplotReact from "uplot-react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { useThemeVersion } from "../lib/theme";

/**
 * A series carries the *name* of a CSS token, not a resolved colour, so its
 * stroke re-resolves on every theme-reactive render.
 */
export type ChartSeries = { name: string; data: number[]; colorVar: string };

/** Read a CSS custom property off the document root (theme-aware). */
export function cssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v === "" ? fallback : v;
}

/**
 * Floating hover readout. uPlot only exposes values through its legend, which
 * we keep non-live, so this is what gives every chart a tooltip.
 */
function tooltipPlugin(format: (v: number) => string): uPlot.Plugin {
  let el: HTMLDivElement | null = null;

  return {
    hooks: {
      init: (u: uPlot) => {
        el = document.createElement("div");
        el.className = "u-tt";
        el.style.display = "none";
        u.over.appendChild(el);
      },
      destroy: () => {
        el?.remove();
        el = null;
      },
      setCursor: (u: uPlot) => {
        if (el === null) return;
        // Narrow once into a local: `el` is a closed-over `let`, so TS can't
        // carry the non-null narrowing into the forEach callback below.
        const box = el;
        const { idx, left, top } = u.cursor;
        if (idx === null || idx === undefined || left === undefined || left < 0) {
          box.style.display = "none";
          return;
        }

        const xs = u.data[0] as number[];
        const when = new Date(xs[idx] * 1000).toLocaleTimeString();

        box.replaceChildren();

        const xRow = document.createElement("div");
        xRow.className = "u-tt-x";
        xRow.textContent = when;
        box.appendChild(xRow);

        u.series.slice(1).forEach((s, i) => {
          const v = (u.data[i + 1] as (number | null | undefined)[])[idx];
          const raw = u.series[i + 1].stroke as unknown;
          const swatch =
            typeof raw === "function"
              ? String((raw as (u: uPlot, i: number) => string)(u, i + 1) ?? "currentColor")
              : typeof raw === "string"
                ? raw
                : "currentColor";
          const shown = v === null || v === undefined ? "—" : format(v);

          const row = document.createElement("div");
          const sw = document.createElement("span");
          sw.className = "u-tt-sw";
          sw.style.background = swatch;
          row.appendChild(sw);
          row.appendChild(document.createTextNode(`${s.label ?? ""} ${shown}`));
          box.appendChild(row);
        });

        box.style.display = "block";

        // Keep the box inside the plot area, flipping side near the right edge.
        const pad = 12;
        const w = box.offsetWidth;
        const h = box.offsetHeight;
        const x = left + pad + w > u.over.clientWidth ? left - pad - w : left + pad;
        const y = Math.min(Math.max((top ?? 0) - h / 2, 0), Math.max(u.over.clientHeight - h, 0));
        box.style.left = `${Math.max(x, 0)}px`;
        box.style.top = `${y}px`;
      },
    },
  };
}

export default function TimeSeriesChart({
  timestamps,
  series,
  height = 200,
  format = (v) => v.toFixed(2),
}: {
  timestamps: number[];
  series: ChartSeries[];
  height?: number;
  format?: (v: number) => string;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  // Re-render (and re-read every cssVar below, including series strokes) on a
  // theme flip.
  const themeVersion = useThemeVersion();

  useEffect(() => {
    const el = box.current;
    if (el === null) return;
    const ro = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const axis = cssVar("--border", "#242424");
  const label = cssVar("--muted-foreground", "#8A8A8A");

  // series carries fresh array/object references every render, so key the
  // memo on the (name, colorVar) pairs that actually determine the uPlot
  // series config rather than the array identity.
  const seriesKey = series.map((s) => `${s.name}|${s.colorVar}`).join(",");

  const options = useMemo<uPlot.Options>(
    () => ({
      width: Math.max(width, 1),
      height,
      cursor: { show: true, y: false, drag: { x: false, y: false } },
      legend: { show: series.length > 1, live: false },
      plugins: [tooltipPlugin(format)],
      scales: { x: { time: true } },
      axes: [
        {
          stroke: label,
          grid: { stroke: axis, width: 1 },
          ticks: { stroke: axis },
          font: '11px "IBM Plex Mono", monospace',
        },
        {
          stroke: label,
          grid: { stroke: axis, width: 1 },
          ticks: { stroke: axis },
          font: '11px "IBM Plex Mono", monospace',
          // The y labels go through the SAME formatter as the tooltip, so a
          // bytes chart reads "150 KB/s", not a raw "150,000" (user-reported:
          // raw values both overflowed the default 50px gutter — clipping to
          // "00,000" — and meant nothing without units).
          values: (_u: uPlot, vals: number[]) => vals.map(format),
          // Size the gutter to the longest label actually rendered instead
          // of uPlot's fixed default: formatted rates ("150.0 KB/s") are far
          // wider than the percentages the default was tuned for. ~6.6px per
          // character at 11px IBM Plex Mono, plus tick+padding.
          size: (_u: uPlot, vals: string[] | null) => {
            const longest = vals === null ? 4 : vals.reduce((m, v) => Math.max(m, v.length), 0);
            return Math.ceil(longest * 6.6) + 18;
          },
        },
      ],
      series: [
        {},
        ...series.map((s) => ({ label: s.name, stroke: cssVar(s.colorVar, "#F5F5F5"), width: 2 })),
      ],
    }),
    [width, height, themeVersion, seriesKey, format],
  );

  const data = [timestamps, ...series.map((s) => s.data)] as uPlot.AlignedData;

  return (
    <div ref={box} className="w-full">
      {width > 0 && <UplotReact key={themeVersion} options={options} data={data} />}
    </div>
  );
}
