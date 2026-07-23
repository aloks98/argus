/**
 * Empty stand-in for echarts.
 *
 * @e412/rnui-react ships one barrel module that does
 * `import * as core from "echarts/core"` + named imports from
 * echarts/{charts,components,renderers,features}, then runs echarts'
 * side-effectful `core.use([...])` registration at module scope. So Rollup
 * cannot tree-shake echarts out even when no chart component is imported — it is
 * ~55% of our bundle. Since we render charts with uPlot and use no rnui chart
 * component, `vite.config.ts` aliases every `echarts*` specifier to this file.
 *
 * It MUST export every name the barrel imports, or Rollup errors on a missing
 * export. The barrel calls only `use`/`registerTheme`/`init`/`graphic` on the
 * core namespace, and all of these are no-ops because chart components — which
 * would call the rest — are never rendered.
 *
 * Remove this (and the alias) once @e412/rnui-react declares
 * `"sideEffects": false` and moves the registration inside its chart module.
 */

// echarts/core namespace API — no-ops; nothing draws.
export function use(_?: unknown): void {}
export function init(): null {
  return null;
}
export function registerTheme(): void {}
export function getInstanceByDom(): null {
  return null;
}
export function connect(): void {}
export function disconnect(): void {}
export function dispose(): void {}
export const graphic = {};

// Every renderer / chart / component / feature the barrel imports by name.
// Each is an inert token; the no-op `use()` above ignores them.
const stub = {};
export const CanvasRenderer = stub;
export const BarChart = stub;
export const EffectScatterChart = stub;
export const GaugeChart = stub;
export const LineChart = stub;
export const PieChart = stub;
export const RadarChart = stub;
export const ScatterChart = stub;
export const AriaComponent = stub;
export const AxisPointerComponent = stub;
export const DataZoomComponent = stub;
export const DataZoomInsideComponent = stub;
export const DataZoomSliderComponent = stub;
export const DatasetComponent = stub;
export const GraphicComponent = stub;
export const GridComponent = stub;
export const LegendComponent = stub;
export const MarkAreaComponent = stub;
export const MarkLineComponent = stub;
export const MarkPointComponent = stub;
export const PolarComponent = stub;
export const RadarComponent = stub;
export const TimelineComponent = stub;
export const TitleComponent = stub;
export const ToolboxComponent = stub;
export const TooltipComponent = stub;
export const TransformComponent = stub;
export const VisualMapComponent = stub;
export const LabelLayout = stub;
export const UniversalTransition = stub;

export default { use, init, registerTheme, getInstanceByDom, graphic };
