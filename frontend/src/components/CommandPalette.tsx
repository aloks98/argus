// Ctrl/Cmd+K palette: machines and their tabs, plus static routes. Entirely
// client-side over the cached fleet list — nothing is fetched on open beyond
// what the fleet page already polls. The fleet query here is enabled only
// while the dialog is open, so mounting the palette app-wide does not add a
// permanent background poll on pages that don't otherwise need the fleet.
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
      {/* The explicit <Command> root is LOAD-BEARING, not decoration: unlike
          shadcn's CommandDialog, rnui's passes children straight into
          DialogContent without providing cmdk's Command context. Every
          CommandInput/List/Empty/Item below reads that context via
          useSyncExternalStore(store.subscribe, ...) — with no root, store is
          undefined and the first open throws "Cannot read properties of
          undefined (reading 'subscribe')", unmounting the entire app to a
          black page. There is no type error either way, only the crash. */}
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
