import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { zodResolver } from "@hookform/resolvers/zod";
import { Controller, useForm } from "react-hook-form";
import * as z from "zod";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  Field,
  FieldError,
  FieldGroup,
  FieldLabel,
  Input,
  Spinner,
} from "@e412/rnui-react";
import { ArrowLeft } from "lucide-react";
import { RateLimited, localLogin } from "../api";

/**
 * Full-page gate: both methods are always offered — this must never probe
 * `/auth/login` to hide whichever isn't configured.
 *
 * `GET /auth/login` is not a status check, it starts a real OIDC flow
 * (design §8/§13): it mints CSRF/nonce/PKCE state and a live flow cookie
 * shared across tabs, so a background probe could stomp a legitimate SSO
 * login in flight in another tab, breaking its callback check.
 *
 * So: no probe. An unconfigured-SSO click just gets the server's own
 * "not configured" page — free, and the only way to know that without
 * ever making a request from here.
 */
export default function SignIn() {
  const next = window.location.pathname + window.location.search;
  // Two stages rather than a disclosure: picking a method replaces the
  // choice instead of growing beneath it — one decision on screen at a time.
  const [stage, setStage] = useState<"choose" | "local">("choose");

  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-6">
      <div className="text-center">
        <div className="font-display text-2xl tracking-widest">ARGUS</div>
        <p className="mt-2 font-mono text-[11px] uppercase tracking-widest text-muted-foreground">
          {stage === "choose" ? "Sign in to continue" : "Local account"}
        </p>
      </div>

      <div className="flex w-full max-w-xs flex-col gap-4">
        {stage === "choose" ? (
          <>
            <Button
              className="w-full"
              onClick={() => {
                window.location.href = `/auth/login?next=${encodeURIComponent(next)}`;
              }}
            >
              Sign in with SSO
            </Button>
            {/* Outline keeps SSO primary without demoting this route to small
                print — in an SSO outage or local-only deployment, this is the
                one that works, and shouldn't require hunting for. */}
            <Button variant="outline" className="w-full" onClick={() => setStage("local")}>
              Use a local account
            </Button>
          </>
        ) : (
          <>
            {/* A plain control carrying breadcrumb weight, not a Button — a
                Button here reads as heavy as the submit control below it.
                Styled to match MachineDetailPage's breadcrumb link so "leave
                this stage" looks the same everywhere.

                Still a real <button>, not <a>: it changes state, not
                navigation, and an href-less <a> isn't focusable or announced
                as a control. `self-start` stops the flex column from
                stretching it full-width. */}
            <button
              type="button"
              onClick={() => setStage("choose")}
              className="flex items-center gap-1.5 self-start font-mono text-[11px] uppercase tracking-widest text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
            >
              <ArrowLeft className="size-3.5" />
              Back
            </button>
            <LocalSignInForm />
          </>
        )}
      </div>
    </div>
  );
}

/**
 * Deliberately validates only PRESENCE. A length rule would mirror
 * `generate_password`'s current output, breaking the moment that changes,
 * and would leak the secret's shape to an unauthenticated visitor. Whether
 * it's *correct* is the server's business — it answers every wrong case
 * identically on purpose (design §11).
 */
const localSignInSchema = z.object({
  username: z.string().min(1, "Enter your username."),
  password: z.string().min(1, "Enter your password."),
});

function LocalSignInForm() {
  const queryClient = useQueryClient();
  const form = useForm<z.infer<typeof localSignInSchema>>({
    resolver: zodResolver(localSignInSchema),
    defaultValues: { username: "", password: "" },
  });

  const mutation = useMutation({
    mutationFn: (values: z.infer<typeof localSignInSchema>) =>
      localLogin(values.username, values.password),
    onSuccess: () => {
      // The server already set the session cookie; invalidating `["me"]` is
      // all that's needed for the `Gate` in main.tsx to re-evaluate and swap
      // this view out for the app.
      void queryClient.invalidateQueries({ queryKey: ["me"] });
    },
  });

  const rateLimited = mutation.error instanceof RateLimited ? mutation.error : null;
  // Anything but rate-limiting renders as ONE generic message — never
  // anything that would reveal whether the account exists (design §11; the
  // server already makes the three failure cases indistinguishable).
  const genericFailure = mutation.error !== null && rateLimited === null;

  return (
    <form
      className="flex flex-col gap-3"
      noValidate
      onSubmit={form.handleSubmit((values) => mutation.mutate(values))}
    >
      {rateLimited !== null && (
        <Alert variant="warning">
          <AlertTitle>Too many attempts</AlertTitle>
          <AlertDescription>Try again in {rateLimited.retryAfterSeconds}s.</AlertDescription>
        </Alert>
      )}
      {genericFailure && (
        <Alert variant="destructive">
          <AlertTitle>Sign-in failed</AlertTitle>
        </Alert>
      )}

      {/* `noValidate` on the form above is deliberate: without it the browser's
          own bubble fires first and FieldError never gets a chance to render —
          the two validation systems would fight and native would always win. */}
      <FieldGroup>
        <Controller
          name="username"
          control={form.control}
          render={({ field, fieldState }) => (
            <Field data-invalid={fieldState.invalid}>
              <FieldLabel htmlFor="local-username">Username</FieldLabel>
              <Input
                {...field}
                id="local-username"
                autoComplete="username"
                aria-invalid={fieldState.invalid}
              />
              {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
            </Field>
          )}
        />
        <Controller
          name="password"
          control={form.control}
          render={({ field, fieldState }) => (
            <Field data-invalid={fieldState.invalid}>
              <FieldLabel htmlFor="local-password">Password</FieldLabel>
              <Input
                {...field}
                id="local-password"
                type="password"
                autoComplete="current-password"
                aria-invalid={fieldState.invalid}
              />
              {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
            </Field>
          )}
        />
      </FieldGroup>

      <Button type="submit" variant="outline" className="w-full" disabled={mutation.isPending}>
        {mutation.isPending ? (
          <>
            <Spinner className="size-3.5" />
            Signing in…
          </>
        ) : (
          "Sign in"
        )}
      </Button>
    </form>
  );
}
