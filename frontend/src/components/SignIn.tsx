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
 * Full-page gate, in two stages: pick a method, then use it. Both methods are
 * always offered -- this never probes `/auth/login` to hide whichever isn't
 * configured.
 *
 * That probe was tried and reverted: `GET /auth/login` is not a status
 * check, it is the start of a real OIDC flow (design §8/§13) -- every hit
 * runs discovery, mints a fresh CSRF token/nonce/PKCE verifier, and sets a
 * live 10-minute flow cookie, which the design explicitly documents as
 * reachable only by top-level navigation. Firing it from a background
 * `fetch` on every sign-in-page mount silently started an unrequested OAuth
 * flow for every signed-out visitor, and -- because the flow cookie is
 * per-origin and shared across tabs -- a probe in one tab could stomp the
 * flow cookie of a legitimate SSO login in flight in another, breaking its
 * CSRF/nonce/PKCE check on return to the callback.
 *
 * So: no probe. When SSO isn't configured, a visitor who clicks "Sign in"
 * gets the server's own friendly "single sign-on is not configured" page --
 * a fine outcome that costs nothing, and the only way to know that requires
 * no request to be made from here at all.
 */
export default function SignIn() {
  const next = window.location.pathname + window.location.search;
  // Two stages rather than a disclosure: picking a method replaces the choice
  // instead of growing beneath it. One decision on screen at a time, and the
  // form is never competing for attention with a button that would abandon it.
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
            {/* Outline keeps SSO the primary route without demoting this one
                into small print. In a local-admin-only deployment SSO is the
                route that does NOT work, and an operator mid-outage should not
                have to hunt for the one that does. */}
            <Button variant="outline" className="w-full" onClick={() => setStage("local")}>
              Use a local account
            </Button>
          </>
        ) : (
          <>
            {/* Above the form, not below it: the way back is the first thing
                you should find if you picked this by mistake. */}
            <Button
              variant="ghost"
              className="w-full justify-start"
              onClick={() => setStage("choose")}
            >
              <ArrowLeft className="size-3.5" />
              Back
            </Button>
            <LocalSignInForm />
          </>
        )}
      </div>
    </div>
  );
}

/**
 * Deliberately validates only PRESENCE. A rule like "at least 24 characters"
 * would mirror what `generate_password` happens to produce today, so it would
 * reject a still-valid credential the moment that length changes — and it
 * would tell an unauthenticated visitor the shape of the secret. Whether the
 * password is *correct* is the server's business, and the server answers every
 * wrong answer identically on purpose (design §11).
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
  // Anything that isn't the rate-limit case renders as ONE generic message --
  // never anything that would tell a caller whether the account exists
  // (design §11; the server itself makes the three failure cases
  // indistinguishable, and this must not undo that).
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
          own bubble fires first and the inline FieldError never gets a chance
          to render, so the two validation systems would fight and the native
          one would always win. */}
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
