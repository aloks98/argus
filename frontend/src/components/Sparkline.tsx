import { memo, useMemo } from "react";
import UplotReact from "uplot-react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { cssVar } from "./TimeSeriesChart";
import { useThemeVersion } from "../lib/theme";

const WIDTH = 72;
const HEIGHT = 20;

/**
 * Inline trend for a fleet row. Decorative — the numeric value sits beside it.
 *
 * Memoized on purpose: the fleet page renders every row twice (table +
 * cards, CSS-swapped) with two sparklines each, so an unmemoized version
 * rebuilds 4 uPlot instances per machine on every keystroke in the filter
 * box. TanStack Query's structural sharing keeps `values` referentially
 * stable across polls when the data hasn't changed, so `memo`'s default
 * shallow compare skips those re-renders too; `themeVersion` (read inside)
 * still forces the redraw a theme flip needs.
 */
const Sparkline = memo(function Sparkline({ values }: { values: number[] }) {
  const themeVersion = useThemeVersion();

  // Options only change on a theme flip (the stroke re-reads a CSS var);
  // a fresh object every render would make uplot-react rebuild the chart.
  // `themeVersion` looks unnecessary to exhaustive-deps because the real
  // dependency is the CSS custom property `cssVar` reads off the DOM, which
  // the linter can't see -- same deliberate keying as TimeSeriesChart's memo.
  const options = useMemo<uPlot.Options>(
    () => ({
      width: WIDTH,
      height: HEIGHT,
      cursor: { show: false },
      legend: { show: false },
      axes: [{ show: false }, { show: false }],
      scales: { x: { time: false } },
      series: [{}, { stroke: cssVar("--chart-1", "#FFE600"), width: 1.5 }],
    }),
    // oxlint-disable-next-line react-hooks/exhaustive-deps
    [themeVersion],
  );
  const data = useMemo(() => [values.map((_, i) => i), values] as uPlot.AlignedData, [values]);

  if (values.length === 0) return <span className="text-muted-foreground">—</span>;

  return (
    <div aria-hidden="true">
      <UplotReact key={themeVersion} options={options} data={data} />
    </div>
  );
});

export default Sparkline;
