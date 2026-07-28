// Identity editing for one machine: display name, tags, notes. All three
// commit through PATCH /api/machines/:id with an explicit Save — nothing
// saves on blur, so a stray edit can't persist silently (design "Machine
// detail"). Tags use an autocomplete-suggested free-text input with the
// fleet-wide vocabulary as suggestions; free entry stays allowed (this is a
// free-form tag field, not a curated list) — see the note on `TagsField`
// below for why this isn't rnui's Combobox chips surface.
import { useEffect, useState } from "react";
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
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
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

export default function MachineIdentity({ machine }: { machine: MachineDetail }) {
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
        },
      },
    );
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>Identity</CardTitle>
        <CardDescription>Rename, tag, and annotate this machine.</CardDescription>
      </CardHeader>
      <CardContent>
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
      </CardContent>
    </Card>
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
 * that's the signal this component uses to commit immediately. Typed text
 * commits on Enter via a plain `onKeyDown`, independent of whatever base-ui
 * highlighted via arrow keys, so the commit path is one function
 * (`commit`) regardless of how the text got into the box.
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

  const commit = (raw: string) => {
    const tag = raw.trim();
    if (tag === "") return;
    if (value.some((t) => t.toLowerCase() === tag.toLowerCase())) {
      setQuery("");
      return;
    }
    onChange([...value, tag]);
    setQuery("");
  };

  const remove = (tag: string) => onChange(value.filter((t) => t !== tag));

  // Suggestions already carried by this machine are noise in its own list.
  const options = suggestions.filter(
    (t) => !value.some((v) => v.toLowerCase() === t.toLowerCase()),
  );

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
      >
        <AutocompleteInput
          id={id}
          placeholder="Add a tag…"
          aria-invalid={invalid}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
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
