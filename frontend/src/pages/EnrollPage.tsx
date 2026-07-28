// The Enroll page (fleet-identity slice, Task 9): mint a join token for a new
// agent, show the raw secret exactly once, and manage existing tokens
// (usage/expiry/revoke). Mirrors SignIn.tsx's form structure (react-hook-form
// + zod via Controller + rnui Field/FieldLabel/FieldError, `noValidate`).
import { useState } from "react";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import type { Resolver } from "react-hook-form";
import * as z from "zod";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  Badge,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Checkbox,
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
  CopyButton,
  EmptyState,
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  Input,
  Spinner,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@e412/rnui-react";
import { ChevronRight } from "lucide-react";
import type { EnrollmentToken, MintTokenBody } from "../api";
import PageHeader from "../components/PageHeader";
import StatusBadge from "../components/StatusBadge";
import { formatRelative } from "../lib/format";
import { useEnrollmentTokens, useMintToken, useRevokeToken } from "../lib/queries";
import { tokenState, tokenTone } from "../lib/status";

/** Given verbatim by the brief. */
const mintSchema = z.object({
  name: z.string().min(1, "Enter a label.").max(64, "At most 64 characters."),
  display_name: z.string().max(64, "At most 64 characters.").optional(),
  tags: z.string().optional(), // comma-separated; parsed on submit
  unlimited_uses: z.boolean(),
  max_uses: z.coerce.number().int().min(1).optional(),
  never_expires: z.boolean(),
  expires_in_hours: z.coerce.number().int().min(1).max(8760).optional(),
});

type MintFormValues = z.infer<typeof mintSchema>;

const defaultMintValues: MintFormValues = {
  name: "",
  display_name: "",
  tags: "",
  unlimited_uses: false,
  max_uses: 1,
  never_expires: false,
  expires_in_hours: 24,
};

function toMintBody(values: MintFormValues): MintTokenBody {
  const tags = (values.tags ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  const display_name = values.display_name?.trim();
  return {
    name: values.name.trim(),
    ...(display_name ? { display_name } : {}),
    tags,
    // Sent explicitly either way — an explicit 1/24 rather than an absent
    // field, since the form always has a value here. The server treats
    // "absent" and "explicit default" identically for the non-null case, so
    // this is only observable in the unlimited/never-expires branch, where
    // the explicit `null` is what actually means unlimited/never.
    max_uses: values.unlimited_uses ? null : (values.max_uses ?? 1),
    expires_in_hours: values.never_expires ? null : (values.expires_in_hours ?? 24),
  };
}

export default function EnrollPage() {
  return (
    <>
      <PageHeader
        title="Enroll"
        meta="Mint a join token so a new agent can enroll into the fleet."
      />
      <div className="flex flex-col gap-4">
        <MintTokenCard />
        <TokenTable />
      </div>
    </>
  );
}

function MintTokenCard() {
  const mintMutation = useMintToken();
  const form = useForm<MintFormValues>({
    // `z.coerce.number()` gives the schema an *input* type of `unknown` for
    // `max_uses`/`expires_in_hours` (any input is accepted pre-coercion), so
    // `zodResolver`'s inferred `Resolver<Input, ..., Output>` doesn't line up
    // with `useForm<MintFormValues>` (the *output*/post-coercion shape this
    // form actually reads and writes everywhere else — `defaultValues`,
    // `Controller`'s `field.value`, `onSubmit`). The cast is safe: at
    // runtime the resolver still coerces exactly per the schema; only the
    // TS-side input/output split (a well-known zod-coerce + RHF typing gap)
    // needed reconciling.
    resolver: zodResolver(mintSchema) as Resolver<MintFormValues>,
    defaultValues: defaultMintValues,
  });

  // Drives disabling the paired number input — checked "unlimited"/"never
  // expires" means whatever is typed in the number field is ignored at
  // submit (see `toMintBody`), so disabling it is honest about that rather
  // than leaving a number field editable but pointless.
  const unlimitedUses = form.watch("unlimited_uses");
  const neverExpires = form.watch("never_expires");

  const onSubmit = (values: MintFormValues) => {
    mintMutation.mutate(toMintBody(values), {
      onSuccess: () => form.reset(defaultMintValues),
    });
  };

  return (
    <>
      <Card>
        <CardHeader>
          <CardTitle>Mint an enrollment token</CardTitle>
          <CardDescription>
            A join token an agent uses once to enroll into the fleet.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {mintMutation.error !== null && (
            <Alert variant="destructive" className="mb-4">
              <AlertTitle>Mint failed</AlertTitle>
              <AlertDescription>{mintMutation.error.message}</AlertDescription>
            </Alert>
          )}

          {/* `noValidate`: see SignIn.tsx's LocalSignInForm for why — without
              it the browser's own bubble validation wins the race against
              FieldError. */}
          <form
            noValidate
            onSubmit={form.handleSubmit(onSubmit)}
            className="flex flex-col gap-4"
          >
            <FieldGroup>
              <Controller
                name="name"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor="mint-name">Label</FieldLabel>
                    <Input
                      {...field}
                      id="mint-name"
                      placeholder="e.g. rack-3-batch"
                      aria-invalid={fieldState.invalid}
                    />
                    <FieldDescription>
                      Identifies this token in the table below — not shown to the agent.
                    </FieldDescription>
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />

              <Controller
                name="display_name"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor="mint-display-name">Display name</FieldLabel>
                    <Input
                      {...field}
                      id="mint-display-name"
                      placeholder="Optional — applied to the enrolled machine"
                      aria-invalid={fieldState.invalid}
                    />
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />

              <Controller
                name="tags"
                control={form.control}
                render={({ field, fieldState }) => (
                  <Field data-invalid={fieldState.invalid}>
                    <FieldLabel htmlFor="mint-tags">Tags</FieldLabel>
                    <Input
                      {...field}
                      id="mint-tags"
                      placeholder="rack-3, prod"
                      aria-invalid={fieldState.invalid}
                    />
                    <FieldDescription>
                      Comma-separated. Applied to the enrolled machine.
                    </FieldDescription>
                    {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                  </Field>
                )}
              />
            </FieldGroup>

            <Collapsible>
              <CollapsibleTrigger className="group flex items-center gap-1.5 self-start font-mono text-[11px] uppercase tracking-widest text-muted-foreground hover:text-foreground">
                <ChevronRight className="size-3.5 transition-transform group-data-panel-open:rotate-90" />
                Advanced
              </CollapsibleTrigger>
              <CollapsibleContent className="mt-3">
                <FieldGroup>
                  <Controller
                    name="max_uses"
                    control={form.control}
                    render={({ field, fieldState }) => (
                      <Field data-invalid={fieldState.invalid}>
                        <FieldLabel htmlFor="mint-max-uses">Max uses</FieldLabel>
                        <div className="flex flex-wrap items-center gap-3">
                          <Input
                            {...field}
                            id="mint-max-uses"
                            type="number"
                            min={1}
                            disabled={unlimitedUses}
                            aria-invalid={fieldState.invalid}
                            className="max-w-[7rem]"
                          />
                          <Controller
                            name="unlimited_uses"
                            control={form.control}
                            render={({ field: unlimitedField }) => (
                              <div className="flex items-center gap-2">
                                <Checkbox
                                  id="mint-unlimited-uses"
                                  checked={unlimitedField.value}
                                  onCheckedChange={unlimitedField.onChange}
                                />
                                <label
                                  htmlFor="mint-unlimited-uses"
                                  className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
                                >
                                  Unlimited
                                </label>
                              </div>
                            )}
                          />
                        </div>
                        <FieldDescription>Default: single use.</FieldDescription>
                        {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                      </Field>
                    )}
                  />

                  <Controller
                    name="expires_in_hours"
                    control={form.control}
                    render={({ field, fieldState }) => (
                      <Field data-invalid={fieldState.invalid}>
                        <FieldLabel htmlFor="mint-expires-in-hours">
                          Expires in (hours)
                        </FieldLabel>
                        <div className="flex flex-wrap items-center gap-3">
                          <Input
                            {...field}
                            id="mint-expires-in-hours"
                            type="number"
                            min={1}
                            max={8760}
                            disabled={neverExpires}
                            aria-invalid={fieldState.invalid}
                            className="max-w-[7rem]"
                          />
                          <Controller
                            name="never_expires"
                            control={form.control}
                            render={({ field: neverField }) => (
                              <div className="flex items-center gap-2">
                                <Checkbox
                                  id="mint-never-expires"
                                  checked={neverField.value}
                                  onCheckedChange={neverField.onChange}
                                />
                                <label
                                  htmlFor="mint-never-expires"
                                  className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground"
                                >
                                  Never expires
                                </label>
                              </div>
                            )}
                          />
                        </div>
                        <FieldDescription>Default: 24 hours.</FieldDescription>
                        {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                      </Field>
                    )}
                  />
                </FieldGroup>
              </CollapsibleContent>
            </Collapsible>

            <div className="flex justify-end">
              <Button type="submit" disabled={mintMutation.isPending}>
                {mintMutation.isPending ? (
                  <>
                    <Spinner className="size-3.5" />
                    Minting…
                  </>
                ) : (
                  "Mint token"
                )}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>

      {/* Component state only (`mintMutation.data`, held by this hook
          instance) — never written to the query cache or anywhere else, so
          navigating away loses the raw token for good. That's the point: the
          server only ever stores its hash (design "Enroll page"). A fresh
          `mutate()` call clears `data` back to `undefined` before the new
          result arrives (see query-core's "pending" reducer case), so
          starting a second mint doesn't leave a stale token on screen. */}
      {mintMutation.isSuccess && mintMutation.data && (
        <ResultPanel data={mintMutation.data} />
      )}
    </>
  );
}

function ResultPanel({ data }: { data: EnrollmentToken & { token: string } }) {
  const runBlock = [
    "sudo -n env \\",
    "  ARGUS_AGENT_ENDPOINT=https://<agent-endpoint>:9443 \\",
    `  ARGUS_JOIN_TOKEN=${data.token} \\`,
    "  ARGUS_CA_CERT=/etc/argus/argus-ca.crt \\",
    "  ARGUS_DATA_DIR=/var/lib/argus-agent \\",
    "  ./argus-agent",
  ].join("\n");

  return (
    <Card>
      <CardHeader>
        <CardTitle>Token minted</CardTitle>
        <CardDescription>
          Shown once. The server stores only a hash.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <div className="flex items-center gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2">
          <code className="min-w-0 flex-1 select-all break-all font-mono text-sm">
            {data.token}
          </code>
          <CopyButton value={data.token} />
        </div>

        <Button
          variant="outline"
          size="sm"
          className="w-fit"
          render={<a href="/api/ca.pem" download="argus-ca.crt" />}
        >
          Download CA certificate
        </Button>

        <div>
          <div className="flex items-center justify-between gap-2 rounded-t-lg border border-b-0 border-border bg-muted/40 px-3 py-1.5">
            <span className="font-mono text-[11px] uppercase tracking-widest text-muted-foreground">
              Run on the host
            </span>
            <CopyButton value={runBlock} />
          </div>
          <pre className="overflow-x-auto rounded-b-lg border border-border bg-muted/20 px-3 py-2 font-mono text-xs">
            {runBlock}
          </pre>
          <FieldDescription className="mt-1">
            Replace <code>&lt;agent-endpoint&gt;</code> with the address agents reach the
            control plane on — Argus cannot know its externally routable address.
          </FieldDescription>
        </div>
      </CardContent>
    </Card>
  );
}

function TokenTable() {
  const tokensQuery = useEnrollmentTokens();
  const revokeMutation = useRevokeToken();
  const tokens = tokensQuery.data ?? [];

  // The row a revoke confirm is pending for; `null` when the AlertDialog is
  // closed. Controlled (not an AlertDialogTrigger per row) so the dialog's
  // content can depend on which row was clicked, same shape as
  // MachineDetailPage's edit Dialog.
  const [revokeTarget, setRevokeTarget] = useState<EnrollmentToken | null>(null);

  function openRevoke(t: EnrollmentToken) {
    revokeMutation.reset();
    setRevokeTarget(t);
  }

  function closeRevoke() {
    setRevokeTarget(null);
    revokeMutation.reset();
  }

  return (
    <>
      <div className="flex flex-wrap items-baseline gap-2 pb-2 pt-2">
        <h2 className="font-display text-sm uppercase tracking-widest">Tokens</h2>
        <span className="font-mono text-[11px] normal-case tracking-normal text-muted-foreground">
          {tokensQuery.isPending
            ? "loading…"
            : `${tokens.length} token${tokens.length === 1 ? "" : "s"}`}
        </span>
      </div>

      {tokensQuery.error != null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Failed to load tokens</AlertTitle>
          <AlertDescription>{tokensQuery.error.message}</AlertDescription>
        </Alert>
      )}

      <div className="border border-border">
        {!tokensQuery.isPending && tokens.length === 0 ? (
          <EmptyState
            title="No tokens yet"
            description="Mint one above to enroll your first agent."
          />
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Label</TableHead>
                <TableHead>Tags</TableHead>
                <TableHead>Uses</TableHead>
                <TableHead>Expires</TableHead>
                <TableHead>State</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {tokens.map((t) => {
                const state = tokenState(t);
                return (
                  <TableRow key={t.id}>
                    <TableCell className="font-medium">
                      <div className="flex flex-col gap-0.5">
                        <span className="font-mono text-sm">{t.name}</span>
                        {t.display_name !== null && (
                          <span className="font-mono text-[11px] text-muted-foreground">
                            {t.display_name}
                          </span>
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      {t.tags.length === 0 ? (
                        "—"
                      ) : (
                        <div className="flex flex-wrap gap-1">
                          {t.tags.map((tag) => (
                            <Badge key={tag} variant="outline">
                              {tag}
                            </Badge>
                          ))}
                        </div>
                      )}
                    </TableCell>
                    <TableCell className="font-mono">
                      {t.uses}/{t.max_uses === null ? "∞" : t.max_uses}
                    </TableCell>
                    <TableCell className="font-mono">{formatRelative(t.expires_at)}</TableCell>
                    <TableCell>
                      <StatusBadge tone={tokenTone(state)} label={state} />
                    </TableCell>
                    <TableCell className="text-right">
                      {state === "active" && (
                        <Button size="sm" variant="outline" onClick={() => openRevoke(t)}>
                          Revoke
                        </Button>
                      )}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </div>

      <AlertDialog
        open={revokeTarget !== null}
        onOpenChange={(open) => {
          if (!open) closeRevoke();
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Revoke this token?</AlertDialogTitle>
            <AlertDialogDescription>
              {revokeTarget !== null && (
                <>
                  <span className="font-mono">{revokeTarget.name}</span> will no longer enroll
                  new machines. This cannot be undone.
                </>
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>

          {revokeMutation.error !== null && (
            <Alert variant="destructive">
              <AlertTitle>Revoke failed</AlertTitle>
              <AlertDescription>{revokeMutation.error.message}</AlertDescription>
            </Alert>
          )}

          <AlertDialogFooter>
            <AlertDialogCancel disabled={revokeMutation.isPending}>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              disabled={revokeMutation.isPending}
              onClick={() => {
                if (revokeTarget === null) return;
                revokeMutation.mutate(revokeTarget.id, {
                  onSuccess: () => setRevokeTarget(null),
                });
              }}
            >
              {revokeMutation.isPending ? (
                <>
                  <Spinner className="size-3.5" />
                  Revoking…
                </>
              ) : (
                "Revoke"
              )}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
