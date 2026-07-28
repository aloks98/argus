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
  // Owns the one `useMintToken()` instance and both dialogs' open state at
  // the page level — the "Mint a token" trigger lives in PageHeader's
  // `actions` slot (browser-review: "move it into the header, far right"),
  // a sibling of `MintDialogs` rather than a descendant, so nothing here can
  // live inside a single subtree the way it used to.
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
 * that used to open them from here now lives in PageHeader's actions slot.
 * State (including the shared `useMintToken()` mutation) is owned by
 * `EnrollPage` and threaded down as props so the header button and these
 * dialogs — no longer in the same subtree — stay in sync without a portal.
 * The mutation still has to live above whichever dialog is currently
 * mounted so `mintMutation.data` survives the form dialog closing (its
 * content, `MintTokenForm`, unmounts on close — see the Dialog below) long
 * enough for the result dialog to read it.
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
          outside-click, and the built-in X all close it) — unlike the result
          dialog below, nothing here is destructive or shown only once.
          `MintTokenForm` is DialogContent's direct child; rnui's
          `DialogContent` wraps `Dialog.Portal` with no `keepMounted`
          (defaults to `false`), so the portal — and `MintTokenForm` with
          it — unmounts once the close transition finishes. That gives the
          form fresh `useForm()` state on every open for free, the same
          mechanism MachineDetailPage's identity Dialog relies on for
          `MachineIdentity` (see its comment for the precedent). */}
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

      {/* Dialog 2: the result. The raw token is shown exactly once (design
          "Enroll page") — an accidental Esc or outside click must not eat
          it, so this is an AlertDialog with an `onOpenChange` guard on top,
          not a plain Dialog:
            - AlertDialog forces `disablePointerDismissal` internally
              (base-ui's `useRenderDialogRoot`: `isAlertDialog ||
              disablePointerDismissalProp`), which already blocks
              outside-press.
            - Escape is NOT gated by that same flag, though — base-ui's
              dismiss wiring (`useDialogRoot`'s `DialogInteractions`) passes
              `escapeKey: isTopmost` unconditionally to `useDismiss`, so an
              AlertDialog closes on Escape exactly like a plain Dialog unless
              something cancels the attempt. Verified by reading both
              `useDialogRoot.js` and `useDismiss.js` in
              `@base-ui/react` — this is not the "AlertDialog blocks Escape
              too" behavior it's easy to assume from the name.
          The guard below calls `eventDetails.cancel()` for every close
          attempt, which base-ui checks *before* touching its own open state
          (`applyPopupOpenChange`: `onOpenChange` runs, then `if
          (eventDetails.isCanceled) return;`), so a canceled Escape/outside
          press never starts an exit transition — no flicker back open. The
          "Done" button below sets `resultOpen` directly, which changes the
          controlled `open` prop rather than going through this dismiss flow
          at all, so it's the only path that actually closes the dialog. */}
      <AlertDialog
        open={resultOpen}
        onOpenChange={(open, eventDetails) => {
          if (!open) eventDetails.cancel();
        }}
      >
        {/* `AlertDialogContent`'s own width classes are
            `data-[size=default]:max-w-xs data-[size=sm]:max-w-xs
            data-[size=default]:sm:max-w-sm` — a compound `data-[size=...]`
            modifier chain, not the plain `sm:` chain a Dialog's width classes
            use. A bare `sm:max-w-lg` override (what this used to say) has a
            *different* modifier chain, so tailwind-merge can't tell it
            conflicts with the base and won't dedupe it — both classes ship,
            and which one wins is down to stylesheet emission order, not
            source order (this is the cascade trap docs/DEV.md's frontend
            design-system section calls out: "write your override with the
            same modifier the base uses"). Spelled with the matching
            `data-[size=default]:` chain here so tailwind-merge actually
            drops the base and this wins deterministically. */}
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
    mintMutation.mutate(toMintBody(values), { onSuccess: onMinted });
  };

  return (
    // No Card here — this component is dialog-only (MintDialogs mounts it
    // inside DialogContent, which already supplies the frame and the
    // visible heading via DialogTitle/DialogDescription). Same reasoning as
    // MachineIdentity.tsx's dialog-only form: a Card wrapper would nest its
    // own border inside the dialog's, rendering as a visible double border.
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
    </>
  );
}

function ResultPanel({ data }: { data: EnrollmentToken & { token: string } }) {
  // `--config` reads the same four keys from an env-file (`KEY=VALUE` per
  // line — a subset of systemd's `EnvironmentFile=` syntax) instead of the
  // process environment (docs/DEV.md, "Enroll an agent" § Alternative). That
  // file survives a reboot, unlike the old `sudo -n env VAR=... ./argus-agent`
  // one-shot, and doubles as a systemd unit's `EnvironmentFile=` unchanged
  // later — the reason this block writes the file first rather than passing
  // the same four values as env vars directly.
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
    // No Card here, same reasoning as MintTokenForm — this panel only ever
    // renders inside the result AlertDialog, which already supplies the
    // frame and the "Token minted" heading via AlertDialogHeader.
    //
    // `min-w-0` here (and `max-w-full` on the boxes below) so this column
    // can never force the dialog wider than its own `max-w-*` — a flex/grid
    // item's default min-width is its content's intrinsic width, which for
    // an unbroken string (the raw token) or a code block is wide enough to
    // blow past the panel and overflow it regardless of how the dialog
    // itself is sized. `CodeBlock` already wraps its code in its own
    // `overflow-x-auto` div and gives its outer wrapper `overflow-hidden`
    // (see @e412/rnui-react's code-block.tsx), which is the other standard
    // fix for this same problem, so the command block below doesn't need a
    // second scrolling wrapper on top of it.
    <div className="flex min-w-0 flex-col gap-4">
      <div className="flex max-w-full items-center gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2">
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
        nativeButton={false}
      >
        Download CA certificate
      </Button>

      <div className="min-w-0 max-w-full">
        {/* vesper: near-black surface with amber accents — the closest bundled
            theme to the app's black-and-hazard-yellow identity; min-light is
            its restrained light-mode counterpart. Any theme named here must
            ALSO be registered in lib/shiki-slim.ts, or the block silently
            renders as plain text. */}
        <CodeBlock
          code={runBlock}
          language="bash"
          showCopy
          title="Run on the host"
          themes={{ light: "min-light", dark: "vesper" }}
        />
        <FieldDescription className="mt-1">
          Replace <code>&lt;agent-endpoint&gt;</code> with the address agents reach the
          control plane on — Argus cannot know its externally routable address.
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
