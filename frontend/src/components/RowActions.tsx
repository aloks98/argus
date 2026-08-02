// The per-row action cluster shared by the Units and Containers tables:
// Logs (read-only, the most-used action) stays a visible button; the three
// mutating verbs collapse behind one menu trigger. One component so the two
// tabs can't drift apart, and so the protected-entity confirm below wraps
// every verb dispatch in exactly one place.
import { useState } from "react";
import { Link } from "react-router-dom";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  Button,
  ButtonGroup,
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
  Spinner,
} from "@e412/rnui-react";
import { EllipsisVertical } from "lucide-react";
import type { Tone } from "../lib/status";
import StatusBadge from "./StatusBadge";

export type Verb = "start" | "stop" | "restart";

/** The last verb's outcome for this row, rendered beside the controls so
 * feedback lands where the click happened — not in a banner the operator
 * has to scroll back up to find. */
export type RowOutcome = { tone: Tone; label: string };

export default function RowActions({
  name,
  logsTo,
  logsDisabledReason,
  active,
  busy,
  busyLabel,
  outcome,
  confirmReason,
  onVerb,
}: {
  /** Entity name — the menu trigger's accessible label and the confirm
   * dialog's title. */
  name: string;
  logsTo: string;
  /** When set, Logs renders disabled with this title and `logsTo` is
   * ignored — a disabled Button, not a disabled Link, since `disabled`
   * does nothing to an anchor and it would still navigate. */
  logsDisabledReason?: string;
  /** Whether the unit/container is currently up — Start disables when it
   * is, Stop when it isn't. Restart is always offered. */
  active: boolean;
  /** A verb is in flight for THIS row: the whole cluster yields to one
   * status line (not a spinner per control) — which verb is running is
   * the useful fact, and a job can take up to 90s. */
  busy: boolean;
  busyLabel: string;
  /** Last outcome for this row (success/failure/unconfirmed), shown inline
   * beside the controls. `null`/absent = no verb has run here. */
  outcome?: RowOutcome | null;
  /** When set, Stop/Restart confirm first and this sentence is the dialog's
   * explanation (why THIS row is guarded). Start never confirms — bringing
   * something up is not the verb that strands a host. */
  confirmReason?: string;
  onVerb: (verb: Verb) => void;
}) {
  // The verb awaiting confirmation; `null` = dialog closed. Row-local state:
  // only one dialog can be open because only one row can be interacted with.
  const [pendingVerb, setPendingVerb] = useState<Extract<Verb, "stop" | "restart"> | null>(null);

  function dispatch(verb: Verb) {
    if (confirmReason !== undefined && verb !== "start") {
      setPendingVerb(verb);
      return;
    }
    onVerb(verb);
  }

  if (busy) {
    return (
      <span
        role="status"
        className="inline-flex items-center gap-2 font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
      >
        <Spinner className="size-3.5" />
        {busyLabel}
      </span>
    );
  }

  return (
    // A flex wrapper (not the cell's `text-right`): ButtonGroup is a
    // block-level `flex w-fit`, so text-align doesn't move it — and the
    // outcome badge needs to sit beside it, not above.
    <div className="flex items-center justify-end gap-3">
      {/* role="status" so the outcome is announced when it lands — with the
          success banner gone, this badge IS the screen-reader feedback. */}
      {outcome != null && (
        <span role="status">
          <StatusBadge tone={outcome.tone} label={outcome.label} />
        </span>
      )}
      <ButtonGroup>
        {logsDisabledReason !== undefined ? (
          // The reason rides in the accessible NAME, not only `title` —
          // screen readers don't reliably announce title on a disabled
          // control.
          <Button
            size="sm"
            variant="outline"
            disabled
            title={logsDisabledReason}
            aria-label={`Logs — ${logsDisabledReason}`}
          >
            Logs
          </Button>
        ) : (
          <Button size="sm" variant="outline" render={<Link to={logsTo} />} nativeButton={false}>
            Logs
          </Button>
        )}
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button
                size="sm"
                variant="outline"
                aria-label={`Actions for ${name}`}
                className="px-1.5"
              />
            }
          >
            <EllipsisVertical className="size-4" />
          </DropdownMenuTrigger>
          {/* All three verbs always render in the same order with inapplicable
            ones disabled, not removed — an item never moves between opens. */}
          <DropdownMenuContent align="end">
            <DropdownMenuItem disabled={active} onClick={() => dispatch("start")}>
              Start
            </DropdownMenuItem>
            <DropdownMenuItem disabled={!active} onClick={() => dispatch("stop")}>
              Stop
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => dispatch("restart")}>Restart</DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </ButtonGroup>

      {/* Same controlled AlertDialog shape as the Enroll page's revoke
          confirm. Content depends on which verb was clicked, so it can't be
          an AlertDialogTrigger per menu item. */}
      <AlertDialog
        open={pendingVerb !== null}
        onOpenChange={(open) => {
          if (!open) setPendingVerb(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {pendingVerb === "stop" ? "Stop" : "Restart"}{" "}
              <span className="font-mono">{name}</span>?
            </AlertDialogTitle>
            <AlertDialogDescription>{confirmReason}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              // Stop is the verb that strands a host — destructive red.
              // Restart is expected to come back — the default action look.
              variant={pendingVerb === "stop" ? "destructive" : undefined}
              onClick={() => {
                if (pendingVerb === null) return;
                onVerb(pendingVerb);
                setPendingVerb(null);
              }}
            >
              {pendingVerb === "stop" ? "Stop" : "Restart"}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
