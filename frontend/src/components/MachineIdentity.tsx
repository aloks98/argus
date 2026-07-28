// Identity editing for one machine: display name, tags, notes. All three
// commit through PATCH /api/machines/:id with an explicit Save — nothing
// saves on blur, so a stray edit can't persist silently (design "Machine
// detail"). Tags use an autocomplete-suggested free-text input with the
// fleet-wide vocabulary as suggestions; free entry stays allowed (this is a
// free-form tag field, not a curated list) — see the note on `TagsField`
// below for why this isn't rnui's Combobox chips surface.
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
          // `useUpdateMachine` already wrote this same payload into the
          // query cache; resetting here just brings the FORM's local state
          // back in sync with the server's (possibly normalized) values.
          form.reset(defaultsFrom(data));
          onSaved?.();
        },
      },
    );
  };

  return (
    // No Card here — this component is dialog-only (MachineDetailPage.tsx
    // mounts it inside DialogContent, which already supplies the frame and
    // the visible heading via DialogTitle/DialogDescription). A Card wrapper
    // previously nested its own border inside the dialog's, rendering as a
    // visible double border.
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
      <form
        noValidate
        onSubmit={form.handleSubmit(onSubmit)}
        className="flex flex-col gap-4"
      >
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
 * A tags chip field built from `Badge` + rnui's `Autocomplete` rather than
 * `Combobox`'s chips surface (`ComboboxChips`/`ComboboxChipsInput`).
 *
 * Why: `Combobox`'s multi-select chips are built on selecting *items from
 * `items`* — base-ui's own docs (`@base-ui/react/docs/react/components/
 * combobox.md`, "Creatable items") show that committing text which doesn't
 * match any item requires a bespoke creation flow (a confirmation `Dialog`,
 * a `creatable` sentinel item, pending-query plumbing) — there's no
 * `freeSolo`-style prop. That's real weight for a field whose whole point is
 * "type anything, autocomplete is just a shortcut," and it's not
 * verifiable from types alone — the compiled-probe lesson this task calls
 * out.
 *
 * `Autocomplete.Root` is the better fit for that exact job: `value` IS the
 * raw input text (no selection model to fight), `items` only drive the
 * suggestion popup, and free text is the default rather than an opt-in.
 * Base UI's `AutocompleteRoot` defaults `fillInputOnItemPress: true`
 * (verified in the compiled `AutocompleteRoot.mjs`, not just the `.d.ts`),
 * so picking a suggestion fires `onValueChange` with `reason: "item-press"` —
 * that's the signal this component uses to commit immediately.
 *
 * Enter must never commit something other than what's visibly
 * highlighted/typed, so this does NOT unconditionally intercept Enter.
 * `ComboboxInput.js` (which `Autocomplete.Input` is built on) already does
 * exactly the right thing on its own: when an item is highlighted it calls
 * `stopEvent` and clicks that item (firing the `item-press` path above); when
 * nothing is highlighted its own comment says "Allow form submission when no
 * item is highlighted" and it deliberately does NOT preventDefault. So this
 * component only needs to cover that second case — commit the raw typed text
 * and preventDefault so Enter doesn't submit the surrounding form — and
 * tracks "is anything highlighted right now" itself via `onItemHighlighted`
 * (a ref, not state: it has to be current at the moment the Enter keydown is
 * read, not delayed a render). When a highlight exists, this component's
 * `onKeyDown` does nothing at all and lets Base UI's own path run.
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
  // The currently keyboard-highlighted suggestion, if any. A ref (not
  // state): `onKeyDown`'s Enter handler needs the value AS OF that keydown,
  // and arrow-key highlight changes must be visible to it synchronously,
  // not a render later.
  const highlightedRef = useRef<string | undefined>(undefined);

  const commit = (raw: string) => {
    // Lowercase up front so the chip shown here always matches what the
    // server will actually store (tags are lowercased server-side per
    // constraints.md) — otherwise a typed "Infra" would render as "Infra"
    // until save silently swapped it to "infra".
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
  // Both sides are server-canonical lowercase (`machine.tags` and
  // `fleetTags()` come straight from the API) plus whatever this component
  // itself has committed, which `commit` already lowercases — so a plain
  // `includes` is enough, no case-folding needed here.
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
          // (`REASONS.itemPress === "item-press"`, see
          // `@base-ui/react/internals/reason-parts.mjs`) even though the
          // exported identifier reads camelCase — easy to get backwards
          // since the type alias name doesn't hint at the literal value.
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
            // Only handle Enter ourselves when NOTHING is highlighted.
            // When something IS highlighted, do nothing here — Base UI's own
            // Enter handling (in ComboboxInput.js) selects it, which fires
            // `onValueChange` with reason "item-press" above. Committing the
            // raw `query` in that case would add whatever's literally typed
            // instead of the visibly-selected suggestion.
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
