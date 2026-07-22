// Minimal inline-SVG sparkline — no charting library. Renders a single
// polyline of `values` scaled into a 100x24 viewBox against `max`.
export default function Sparkline({
  values,
  max = 100,
}: {
  values: number[];
  max?: number;
}) {
  if (values.length < 2) return null;
  const w = 100,
    h = 24;
  const pts = values
    .map((v, i) => {
      const x = (i / (values.length - 1)) * w;
      const y = h - (Math.max(0, Math.min(v, max)) / max) * h;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg
      viewBox={`0 0 ${w} ${h}`}
      width={w}
      height={h}
      preserveAspectRatio="none"
      role="img"
      aria-label="sparkline"
    >
      <polyline
        points={pts}
        fill="none"
        stroke="currentColor"
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
