// Logs as a modal dialog over the machine page. A dialog (not a drawer) on
// purpose: vaul's drawer disables text selection and captures mouse-drag to
// dismiss, breaking select-and-copy of log text; a dialog has neither.
//
// The open source lives in the URL (`?logs=journal:nginx.service`, matching
// `?tab=`) so the view survives a reload and is linkable; closing removes it.
import { useParams, useSearchParams } from "react-router-dom";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@e412/rnui-react";
import { ALL_LOGS } from "../api";
import { useLogFilters } from "../lib/logFilters";
import LogFilterBar from "./LogFilterBar";
import LogViewer from "./LazyLogViewer";

export default function LogDialog() {
  const { id } = useParams<{ id: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  const source = searchParams.get("logs");
  const open = source !== null && id !== undefined;

  // Unfiltered by default so the per-unit view behaves exactly as it always has.
  const [filters, setFilters] = useLogFilters(ALL_LOGS);

  function close() {
    const next = new URLSearchParams(searchParams);
    next.delete("logs");
    next.delete("priority");
    next.delete("window");
    setSearchParams(next, { replace: true });
  }

  return (
    <Dialog open={open} onOpenChange={(next: boolean) => !next && close()}>
      {/* `sm:max-w-6xl` (not just `max-w-6xl`): DialogContent's default caps width
          at `sm:max-w-sm`, and tailwind-merge can't dedupe an unprefixed utility
          against that `sm:` one — matching the modifier is what lets ours win. */}
      <DialogContent className="flex h-[85vh] w-[92vw] max-w-6xl flex-col gap-0 p-0 sm:max-w-6xl">
        <DialogHeader className="border-b border-border p-4 text-left">
          <DialogTitle className="font-mono text-sm">{source ?? ""}</DialogTitle>
          <DialogDescription className="font-mono text-[11px]">
            Live tail — closing this stops it on the agent. Drag to select and copy.
          </DialogDescription>
        </DialogHeader>
        <div className="flex min-h-0 flex-1 flex-col p-4">
          {source?.startsWith("journal:") && <LogFilterBar value={filters} onChange={setFilters} />}
          {open && id !== undefined && source !== null && (
            <div className="min-h-0 flex-1">
              <LogViewer machineId={id} source={source} filters={filters} />
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
