// Logs as a modal dialog over the machine page. A dialog (not a drawer) on
// purpose: vaul's drawer disables text selection and captures mouse-drag to
// dismiss, which made the log text unselectable — a dialog has neither, so
// native select-and-copy of a few lines just works. One surface, no separate
// full-page route.
//
// The open source lives in the URL (`?logs=journal:nginx.service`) so it
// survives a reload and is linkable, matching the `?tab=` convention. Closing
// removes the param.
import { useParams, useSearchParams } from "react-router-dom";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@e412/rnui-react";
import LogViewer from "./LogViewer";

export default function LogDialog() {
  const { id } = useParams<{ id: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  const source = searchParams.get("logs");
  const open = source !== null && id !== undefined;

  function close() {
    const next = new URLSearchParams(searchParams);
    next.delete("logs");
    setSearchParams(next, { replace: true });
  }

  return (
    <Dialog open={open} onOpenChange={(next: boolean) => !next && close()}>
      {/* `sm:max-w-6xl` (not just `max-w-6xl`): DialogContent's default caps
          width at `sm:max-w-sm`, and tailwind-merge can't dedupe an unprefixed
          utility against that `sm:` one, so the narrow default would win at
          ≥640px. Matching the modifier is what lets ours take effect. */}
      <DialogContent className="flex h-[85vh] w-[92vw] max-w-6xl flex-col gap-0 p-0 sm:max-w-6xl">
        <DialogHeader className="border-b-2 border-border p-4 text-left">
          <DialogTitle className="font-mono text-sm">{source ?? ""}</DialogTitle>
          <DialogDescription className="font-mono text-[11px]">
            Live tail — closing this stops it on the agent. Drag to select and copy.
          </DialogDescription>
        </DialogHeader>
        <div className="min-h-0 flex-1 p-4">
          {open && id !== undefined && source !== null && (
            <LogViewer machineId={id} source={source} />
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
