import UplotReact from "uplot-react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { cssVar } from "./TimeSeriesChart";
import { useThemeVersion } from "../lib/theme";

const WIDTH = 72;
const HEIGHT = 20;

/** Inline trend for a fleet row. Decorative — the numeric value sits beside it. */
export default function Sparkline({ values }: { values: number[] }) {
  const themeVersion = useThemeVersion();
  if (values.length === 0) return <span className="text-muted-foreground">—</span>;

  const options: uPlot.Options = {
    width: WIDTH,
    height: HEIGHT,
    cursor: { show: false },
    legend: { show: false },
    axes: [{ show: false }, { show: false }],
    scales: { x: { time: false } },
    series: [{}, { stroke: cssVar("--chart-1", "#FFE600"), width: 1.5 }],
  };
  const data = [values.map((_, i) => i), values] as uPlot.AlignedData;

  return (
    <div aria-hidden="true">
      <UplotReact key={themeVersion} options={options} data={data} />
    </div>
  );
}
