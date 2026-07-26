import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
  Field,
  FieldGroup,
  FieldLabel,
  Input,
  Spinner,
} from "@e412/rnui-react";
import { ChevronDown } from "lucide-react";
import { RateLimited, localLogin } from "../api";

/**
 * Full-page gate. Always shows BOTH affordances -- the SSO button and the
 * local-account disclosure -- rather than probing `/auth/login` to hide
 * whichever isn't configured.
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

  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-6">
      <div className="text-center">
        <div className="font-display text-2xl tracking-widest">ARGUS</div>
        <p className="mt-2 font-mono text-[11px] uppercase tracking-widest text-muted-foreground">
          Sign in to continue
        </p>
      </div>

      <div className="flex w-full max-w-xs flex-col gap-4">
        <Button
          className="w-full"
          onClick={() => {
            window.location.href = `/auth/login?next=${encodeURIComponent(next)}`;
          }}
        >
          Sign in with SSO
        </Button>

        <Collapsible>
          {/* Rendered AS a Button so the two ways in read as two affordances of
              equal weight, rather than a button plus a piece of small print.
              That matters here: in a local-admin-only deployment SSO is the
              route that does not work, and an operator mid-outage should not
              have to notice a text link to find the one that does.

              `render` is base-ui's composition prop — the same one AppShell
              uses to render a menu button as a NavLink. The trigger keeps its
              own click handling and `data-panel-open` state and simply borrows
              the Button's styling, so `group-data-[panel-open]` still drives
              the chevron. `outline` rather than the default keeps SSO visually
              primary without demoting this one out of sight. */}
          <CollapsibleTrigger className="group w-full" render={<Button variant="outline" />}>
            Use a local account
            <ChevronDown className="size-3.5 transition-transform group-data-[panel-open]:rotate-180" />
          </CollapsibleTrigger>
          <CollapsibleContent>
            <LocalSignInForm />
          </CollapsibleContent>
        </Collapsible>
      </div>
    </div>
  );
}

function LocalSignInForm() {
  const queryClient = useQueryClient();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");

  const mutation = useMutation({
    mutationFn: () => localLogin(username, password),
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
      className="mt-1 flex flex-col gap-3 border-t border-border pt-3"
      onSubmit={(e) => {
        e.preventDefault();
        mutation.mutate();
      }}
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

      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="local-username">Username</FieldLabel>
          <Input
            id="local-username"
            name="username"
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            required
          />
        </Field>
        <Field>
          <FieldLabel htmlFor="local-password">Password</FieldLabel>
          <Input
            id="local-password"
            name="password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </Field>
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
