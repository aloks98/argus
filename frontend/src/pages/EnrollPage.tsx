// The Enroll page: mint a join token for a new agent, show the raw secret
// exactly once, and manage existing tokens (usage/expiry/revoke). Mirrors
// SignIn.tsx's form structure (react-hook-form + zod, rnui Field/FieldError).
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
  Checkbox,
  CodeBlock,
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
  CopyButton,
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
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
import { codeHighlighter } from "../lib/codeHighlighter";
import { formatRelative } from "../lib/format";
import { useEnrollmentTokens, useMintToken, useRevokeToken } from "../lib/queries";
import { tokenState, tokenTone } from "../lib/status";

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
    // Sent explicitly either way: the server treats absent and explicit-default
    // the same for non-null values, but explicit `null` is what means
    // unlimited/never — only observable in the unlimited/never-expires branch.
    max_uses: values.unlimited_uses ? null : (values.max_uses ?? 1),
    expires_in_hours: values.never_expires ? null : (values.expires_in_hours ?? 24),
  };
}

export default function EnrollPage() {
  // Owns the one `useMintToken()` instance and both dialogs' open state —
  // the "Mint a token" trigger lives in PageHeader's `actions` slot, a
  // sibling of `MintDialogs` rather than a descendant.
  const mintMutation = useMintToken();
  const [mintOpen, setMintOpen] = useState(false);
  const [resultOpen, setResultOpen] = useState(false);

  function openMint() {
    // Clears a previous attempt's error/data before the form (re)mounts —
    // same reason `TokenTable`'s `openRevoke` resets its mutation before
    // opening.
    mintMutation.reset();
    setMintOpen(true);
  }

  return (
    <>
      <PageHeader
        title="Enroll"
        meta="Mint a join token so a new agent can enroll into the fleet."
        actions={<Button onClick={openMint}>Mint a token</Button>}
      />
      <div className="flex flex-col gap-4">
        <MintDialogs
          mintMutation={mintMutation}
          mintOpen={mintOpen}
          setMintOpen={setMintOpen}
          resultOpen={resultOpen}
          setResultOpen={setResultOpen}
        />
        <TokenTable />
      </div>
    </>
  );
}

/**
 * The two mint dialogs (form + result) only — the "Mint a token" trigger
 * lives in PageHeader's `actions` slot. State (including the shared
 * `useMintToken()` mutation) is owned by `EnrollPage` so the header button
 * and these dialogs, not in the same subtree, stay in sync without a
 * portal. The mutation must live above whichever dialog is mounted so
 * `mintMutation.data` survives the form dialog closing (it unmounts on
 * close) long enough for the result dialog to read it.
 */
function MintDialogs({
  mintMutation,
  mintOpen,
  setMintOpen,
  resultOpen,
  setResultOpen,
}: {
  mintMutation: ReturnType<typeof useMintToken>;
  mintOpen: boolean;
  setMintOpen: (open: boolean) => void;
  resultOpen: boolean;
  setResultOpen: (open: boolean) => void;
}) {
  return (
    <>
      {/* Dialog 1: the mint form. A normal dismissable dialog (Escape,
          outside-click, X all close it) — unlike the result dialog below,
          nothing here is destructive or shown only once. rnui's
          `DialogContent` wraps `Dialog.Portal` with no `keepMounted`, so it
          — and `MintTokenForm` with it — unmounts on close, giving the form
          fresh `useForm()` state on every open (same mechanism as
          MachineDetailPage's identity Dialog). */}
      <Dialog open={mintOpen} onOpenChange={setMintOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Mint an enrollment token</DialogTitle>
            <DialogDescription>
              A join token an agent uses once to enroll into the fleet.
            </DialogDescription>
          </DialogHeader>
          <MintTokenForm
            mintMutation={mintMutation}
            onMinted={() => {
              setMintOpen(false);
              setResultOpen(true);
            }}
          />
        </DialogContent>
      </Dialog>

      {/* Dialog 2: the result. The raw token shows exactly once, so an
          accidental Esc/outside-click must not close it — hence AlertDialog
          (blocks outside-press via `disablePointerDismissal`) plus an
          `onOpenChange` guard that cancels every close attempt: AlertDialog
          does NOT block Escape on its own (easy to assume from the name).
          Canceling runs before base-ui touches its own open state, so no
          flicker back open. The "Done" button sets `resultOpen` directly,
          bypassing this guard — the only path that actually closes it. */}
      <AlertDialog
        open={resultOpen}
        onOpenChange={(open, eventDetails) => {
          if (!open) eventDetails.cancel();
        }}
      >
        {/* `AlertDialogContent`'s width classes use a compound
            `data-[size=default]:...` chain, not a plain `sm:` chain — an
            override like `sm:max-w-lg` can't be recognized as conflicting,
            so both classes ship and stylesheet emission order decides which
            wins (see docs/DEV.md's cascade-trap section). Match the base's
            chain here so tailwind-merge drops it and this wins for real. */}
        <AlertDialogContent className="data-[size=default]:max-w-sm data-[size=default]:sm:max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>Token minted</AlertDialogTitle>
            <AlertDialogDescription>
              Shown once. The server stores only a hash.
            </AlertDialogDescription>
          </AlertDialogHeader>

          {mintMutation.data && <ResultPanel data={mintMutation.data} />}

          <AlertDialogFooter>
            {/* Must stay a plain Button, not AlertDialogCancel — that routes
                through the `onOpenChange` guard above, which would cancel
                Done's own close too (dead button). */}
            <Button onClick={() => setResultOpen(false)}>Done</Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

function MintTokenForm({
  mintMutation,
  onMinted,
}: {
  mintMutation: ReturnType<typeof useMintToken>;
  onMinted: () => void;
}) {
  const form = useForm<MintFormValues>({
    // `z.coerce.number()` gives `max_uses`/`expires_in_hours` an *input*
    // type of `unknown` (pre-coercion), so `zodResolver`'s inferred
    // `Resolver<Input, ..., Output>` doesn't match `useForm<MintFormValues>`
    // (the *output* shape used everywhere else). The cast is safe: at
    // runtime the resolver still coerces per the schema — only the TS-side
    // input/output split (a known zod-coerce + RHF typing gap) needs it.
    resolver: zodResolver(mintSchema) as Resolver<MintFormValues>,
    defaultValues: defaultMintValues,
  });

  // Drives disabling the paired number input — checked "unlimited"/"never
  // expires" means the typed value is ignored at submit (see `toMintBody`),
  // so disabling it is honest about that rather than pointless-but-editable.
  const unlimitedUses = form.watch("unlimited_uses");
  const neverExpires = form.watch("never_expires");

  const onSubmit = (values: MintFormValues) => {
    mintMutation.mutate(toMintBody(values), { onSuccess: onMinted });
  };

  return (
    // No Card here — dialog-only (MintDialogs mounts it inside
    // DialogContent, which already supplies the frame/heading). Same
    // reasoning as MachineIdentity.tsx's dialog-only form.
    <>
      {mintMutation.error !== null && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>Mint failed</AlertTitle>
          <AlertDescription>{mintMutation.error.message}</AlertDescription>
        </Alert>
      )}

      {/* `noValidate`: see SignIn.tsx's LocalSignInForm for why — without
          it the browser's own bubble validation wins the race against
          FieldError. */}
      <form noValidate onSubmit={form.handleSubmit(onSubmit)} className="flex flex-col gap-4">
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
                    <FieldLabel htmlFor="mint-expires-in-hours">Expires in (hours)</FieldLabel>
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
    </>
  );
}

function ResultPanel({ data }: { data: EnrollmentToken & { token: string } }) {
  // `--config` reads the same four keys from an env-file (`KEY=VALUE`, a
  // subset of systemd's `EnvironmentFile=` syntax) instead of the process
  // environment (docs/DEV.md). The file survives a reboot and doubles
  // unchanged as a systemd unit's `EnvironmentFile=` later.
  const runBlock = [
    "sudo tee /etc/argus/agent.env <<'EOF'",
    "ARGUS_AGENT_ENDPOINT=https://<agent-endpoint>:9443",
    `ARGUS_JOIN_TOKEN=${data.token}`,
    "ARGUS_CA_CERT=/etc/argus/argus-ca.crt",
    "ARGUS_DATA_DIR=/var/lib/argus-agent",
    "EOF",
    // `tee` writes at the default umask (644) — world-readable, with a real
    // fleet-enrollment credential inside. Lock it down before anything runs.
    "sudo chmod 600 /etc/argus/agent.env",
    "",
    "sudo -n ./argus-agent --config /etc/argus/agent.env",
  ].join("\n");

  return (
    // No Card here, same reasoning as MintTokenForm — this panel only
    // renders inside the result AlertDialog, which already supplies the
    // frame/heading.
    //
    // `min-w-0` (and `max-w-full` below) stops this column forcing the
    // dialog wider than its own `max-w-*`: a flex/grid item's default
    // min-width is its content's intrinsic width, and an unbroken token
    // string is wide enough to overflow regardless of dialog sizing.
    // `CodeBlock` already wraps in `overflow-x-auto`, so the command block
    // below doesn't need a second scrolling wrapper.
    <div className="flex min-w-0 flex-col gap-4">
      <div className="flex max-w-full items-center gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2">
        <code className="min-w-0 flex-1 select-all break-all font-mono text-sm">{data.token}</code>
        <CopyButton value={data.token} />
      </div>

      <Button
        variant="outline"
        size="sm"
        className="w-fit"
        render={<a href="/api/ca.pem" download="argus-ca.crt" />}
        nativeButton={false}
      >
        Download CA certificate
      </Button>

      <div className="min-w-0 max-w-full">
        {/* vesper: near-black surface with amber accents, closest bundled
            theme to the app's identity; min-light is its light-mode
            counterpart. Any theme named here must ALSO be registered in
            lib/codeHighlighter.ts, or the block renders empty. */}
        <CodeBlock
          code={runBlock}
          language="bash"
          showCopy
          title="Run on the host"
          themes={{ light: "min-light", dark: "vesper" }}
          highlighter={codeHighlighter}
        />
        <FieldDescription className="mt-1">
          Replace <code>&lt;agent-endpoint&gt;</code> with the address agents reach the control
          plane on — Argus cannot know its externally routable address.
        </FieldDescription>
      </div>
    </div>
  );
}

function TokenTable() {
  const tokensQuery = useEnrollmentTokens();
  const revokeMutation = useRevokeToken();
  const tokens = tokensQuery.data ?? [];

  // The row a revoke confirm is pending for; `null` when the AlertDialog is
  // closed. Controlled (not an AlertDialogTrigger per row) so the dialog's
  // content can depend on which row was clicked.
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
                  <span className="font-mono">{revokeTarget.name}</span> will no longer enroll new
                  machines. This cannot be undone.
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
