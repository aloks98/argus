// Ctrl/Cmd+K palette: machines and their tabs, plus static routes. Entirely
// client-side over the cached fleet list (see useFleet's `enabled` doc for
// why the query only runs while the dialog is open).
import { useEffect } from "react";
import { useNavigate } from "react-router-dom";
import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@e412/rnui-react";
import { paletteEntries } from "../lib/fleet";
import { useFleet } from "../lib/queries";

export default function CommandPalette({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const { data: rows = [] } = useFleet({ enabled: open });

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        onOpenChange(!open);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onOpenChange]);

  function go(to: string) {
    onOpenChange(false);
    navigate(to);
  }

  const entries = paletteEntries(rows);
  return (
    <CommandDialog open={open} onOpenChange={onOpenChange}>
      {/* The explicit <Command> root is LOAD-BEARING, not decoration: rnui's
          CommandDialog (unlike shadcn's) passes children straight into
          DialogContent without cmdk's Command context, so every
          CommandInput/List/Empty/Item throws "Cannot read properties of
          undefined (reading 'subscribe')" on first open — no type error,
          just a crash to a blank app. */}
      <Command>
        <CommandInput placeholder="Jump to a machine…" />
        <CommandList>
          <CommandEmpty>No matches.</CommandEmpty>
          <CommandGroup heading="Machines">
            {entries.map((e) => (
              <CommandItem key={e.key} value={`${e.label} ${e.keywords}`} onSelect={() => go(e.to)}>
                <span>{e.label}</span>
                <span className="ml-auto font-mono text-[11px] text-muted-foreground">
                  {e.hint}
                </span>
              </CommandItem>
            ))}
          </CommandGroup>
          <CommandGroup heading="Pages">
            <CommandItem value="fleet machines" onSelect={() => go("/machines")}>
              Fleet
            </CommandItem>
            <CommandItem value="enroll token" onSelect={() => go("/enroll")}>
              Enroll a machine
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </Command>
    </CommandDialog>
  );
}
