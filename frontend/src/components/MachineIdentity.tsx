// Identity editing for one machine: display name, tags, notes. All three
// commit through PATCH /api/machines/:id with an explicit Save — nothing
// saves on blur, so a stray edit can't persist silently.
import { useEffect, useRef, useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Autocomplete,
  AutocompleteContent,
  AutocompleteEmpty,
  AutocompleteInput,
  AutocompleteItem,
  AutocompleteList,
  Badge,
  Button,
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  Input,
  Spinner,
  Textarea,
} from "@e412/rnui-react";
import { X } from "lucide-react";
import type { MachineDetail } from "../api";
import { fleetTags } from "../lib/fleet";
import { useFleet, useUpdateMachine } from "../lib/queries";

const identitySchema = z.object({
  display_name: z.string().max(64, "At most 64 characters.").optional(),
  tags: z.array(z.string()).max(16, "At most 16 tags."),
  notes: z.string().max(4000, "At most 4000 characters.").optional(),
});

type IdentityFormValues = z.infer<typeof identitySchema>;

function defaultsFrom(m: MachineDetail): IdentityFormValues {
  return { display_name: m.display_name ?? "", tags: m.tags, notes: m.notes ?? "" };
}

/** `""` means "cleared" on the wire: the server treats an empty string the
 * same as omission (trims, then null), but sending `""` explicitly is what
 * actually clears a previously-set value rather than leaving it untouched. */
function emptyToNull(s: string | undefined): string | null {
  return s === undefined || s === "" ? null : s;
}

export default function MachineIdentity({
  machine,
  onSaved,
}: {
  machine: MachineDetail;
  /** Called after a successful save (form already reset by then). The
   * Dialog host uses this to close itself — see MachineDetailPage.tsx. */
  onSaved?: () => void;
}) {
  const fleetQuery = useFleet();
  const fleetRows = fleetQuery.data ?? [];
  const suggestions = fleetTags(fleetRows).map((t) => t.tag);

  const form = useForm<IdentityFormValues>({
    resolver: zodResolver(identitySchema),
    defaultValues: defaultsFrom(machine),
  });

  // Reset to the server's row whenever the ROUTE changes machines, not on
  // every poll of the same machine — otherwise a background refetch mid-edit
  // would silently discard whatever the operator is typing.
  useEffect(() => {
    form.reset(defaultsFrom(machine));
    // oxlint-disable-next-line react-hooks/exhaustive-deps
  }, [machine.id]);

  const mutation = useUpdateMachine(machine.id);

  const onSubmit = (values: IdentityFormValues) => {
    mutation.mutate(
      {
        display_name: emptyToNull(values.display_name),
        notes: emptyToNull(values.notes),
        tags: values.tags,
      },
      {
        onSuccess: (data) => {
          // `useUpdateMachine` already wrote this into the query cache;
          // resetting here syncs the FORM's local state with the server's
          // (possibly normalized) values.
          form.reset(defaultsFrom(data));
          onSaved?.();
        },
      },
    );
  };

  return (
    // No Card here — this is dialog-only (MachineDetailPage.tsx mounts it
    // inside DialogContent, which already supplies the frame). A Card
    // wrapper would nest its own border, rendering as a double border.
    <>
      {mutation.error !== null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Update failed</AlertTitle>
          <AlertDescription>{mutation.error.message}</AlertDescription>
        </Alert>
      )}

      {/* `noValidate`: without it the browser's own bubble validation
          fires first and FieldError never gets a chance to render — see
          SignIn.tsx's LocalSignInForm for the same note. */}
      <form noValidate onSubmit={form.handleSubmit(onSubmit)} className="flex flex-col gap-4">
        <FieldGroup>
          <Controller
            name="display_name"
            control={form.control}
            render={({ field, fieldState }) => (
              <Field data-invalid={fieldState.invalid}>
                <FieldLabel htmlFor="identity-display-name">Display name</FieldLabel>
                <Input
                  {...field}
                  id="identity-display-name"
                  placeholder={machine.hostname}
                  aria-invalid={fieldState.invalid}
                />
                <FieldDescription>
                  Leave blank to fall back to the hostname ({machine.hostname}).
                </FieldDescription>
                {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
              </Field>
            )}
          />

          <Controller
            name="tags"
            control={form.control}
            render={({ field, fieldState }) => (
              <Field data-invalid={fieldState.invalid}>
                <FieldLabel htmlFor="identity-tags">Tags</FieldLabel>
                <TagsField
                  id="identity-tags"
                  value={field.value}
                  onChange={field.onChange}
                  suggestions={suggestions}
                  invalid={fieldState.invalid}
                />
                <FieldDescription>
                  {field.value.length}/16. Type a tag and press Enter, or pick a suggestion.
                </FieldDescription>
                {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
              </Field>
            )}
          />

          <Controller
            name="notes"
            control={form.control}
            render={({ field, fieldState }) => (
              <Field data-invalid={fieldState.invalid}>
                <FieldLabel htmlFor="identity-notes">Notes</FieldLabel>
                <Textarea
                  {...field}
                  id="identity-notes"
                  rows={4}
                  aria-invalid={fieldState.invalid}
                />
                {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
              </Field>
            )}
          />
        </FieldGroup>

        <div className="flex justify-end">
          <Button type="submit" disabled={mutation.isPending}>
            {mutation.isPending ? (
              <>
                <Spinner className="size-3.5" />
                Saving…
              </>
            ) : (
              "Save"
            )}
          </Button>
        </div>
      </form>
    </>
  );
}

/**
 * Chips built on `Badge` + rnui's `Autocomplete`, not `Combobox` (its chips
 * select only from `items`, no freeSolo path for arbitrary typed text).
 * `Autocomplete`'s `value` IS the raw input text; picking a suggestion fires
 * `onValueChange` with `reason: "item-press"` (Base UI's
 * `fillInputOnItemPress` default) — the signal this component commits on.
 *
 * Enter must never commit something other than what's visibly
 * highlighted/typed. `ComboboxInput` already selects a highlighted item and
 * skips `preventDefault` otherwise (to allow submit) — so this only handles
 * that second case: commit typed text via `highlightedRef` (a ref, since
 * Enter's keydown needs the value synchronously, not a render later).
 */
function TagsField({
  id,
  value,
  onChange,
  suggestions,
  invalid,
}: {
  id: string;
  value: string[];
  onChange: (next: string[]) => void;
  suggestions: string[];
  invalid?: boolean;
}) {
  const [query, setQuery] = useState("");
  // Keyboard-highlighted suggestion, if any — see the doc comment above for
  // why this is a ref, not state.
  const highlightedRef = useRef<string | undefined>(undefined);

  const commit = (raw: string) => {
    // Lowercase up front so the chip matches what the server stores (tags
    // are lowercased server-side, constraints.md) — otherwise a typed
    // "Infra" would flash then silently swap to "infra" after save.
    const tag = raw.trim().toLowerCase();
    if (tag === "") return;
    if (value.includes(tag)) {
      setQuery("");
      return;
    }
    onChange([...value, tag]);
    setQuery("");
    highlightedRef.current = undefined;
  };

  const remove = (tag: string) => onChange(value.filter((t) => t !== tag));

  // Suggestions already carried by this machine are noise in its own list.
  // Both sides are already lowercase (server-canonical `machine.tags`/
  // `fleetTags()`, and `commit` lowercases what this component adds) —
  // plain `includes` is enough, no case-folding needed.
  const options = suggestions.filter((t) => !value.includes(t));

  return (
    <div className="flex flex-col gap-2">
      {value.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {value.map((tag) => (
            <Badge key={tag} variant="outline" className="gap-1 py-0.5 pr-1">
              {tag}
              <button
                type="button"
                onClick={() => remove(tag)}
                aria-label={`Remove tag ${tag}`}
                className="rounded-sm opacity-60 hover:opacity-100"
              >
                <X className="size-3" />
              </button>
            </Badge>
          ))}
        </div>
      )}

      <Autocomplete
        items={options}
        value={query}
        onValueChange={(next, details) => {
          setQuery(next);
          // Base UI's reason strings are kebab-case at runtime
          // (`REASONS.itemPress === "item-press"`) even though the exported
          // identifier reads camelCase — easy to get backwards.
          if (details.reason === "item-press") commit(next);
        }}
        onItemHighlighted={(highlighted) => {
          highlightedRef.current = highlighted;
        }}
      >
        <AutocompleteInput
          id={id}
          placeholder="Add a tag…"
          aria-invalid={invalid}
          onKeyDown={(e) => {
            // Only handle Enter when nothing is highlighted — see this
            // component's doc comment above for why.
            if (e.key === "Enter" && highlightedRef.current === undefined) {
              e.preventDefault();
              commit(query);
            }
          }}
        />
        <AutocompleteContent>
          <AutocompleteEmpty>No matching tags.</AutocompleteEmpty>
          <AutocompleteList>
            {(tag: string) => (
              <AutocompleteItem key={tag} value={tag}>
                {tag}
              </AutocompleteItem>
            )}
          </AutocompleteList>
        </AutocompleteContent>
      </Autocomplete>
    </div>
  );
}
