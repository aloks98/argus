// LogViewer drags in react-logviewer (virtua, ansi parsing) — needed only
// once an operator actually opens a log surface (the Logs tab, or the
// per-unit/container LogDialog), and base-ui unmounts inactive tab panels,
// so a lazy binding defers the fetch until first use. Both consumers go
// through this ONE wrapper so the Suspense fallback shape lives in one
// place; the chunk itself is shared either way (same module specifier).
import { Suspense, lazy, type ComponentProps } from "react";

const LogViewer = lazy(() => import("./LogViewer"));

export default function LazyLogViewer(props: ComponentProps<typeof LogViewer>) {
  return (
    <Suspense
      fallback={<p className="p-2 font-mono text-xs text-muted-foreground">Loading log viewer…</p>}
    >
      <LogViewer {...props} />
    </Suspense>
  );
}
