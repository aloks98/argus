import { useEffect, useState } from "react";
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
 * Probes whether SSO is configured, so the primary button isn't a dead click
 * when it isn't (local-admin design §12 nuance: `/auth/login` now 404s when
 * OIDC is absent, a supported deployment shape).
 *
 * `GET /auth/login` redirects (302/303) to the IdP when OIDC IS configured,
 * and answers 404 when it is not (`crate::auth::oidc::login`). A
 * `redirect: "manual"` fetch turns the redirect case into an opaque,
 * unreadable response instead of following it -- that opacity is exactly the
 * signal used below to tell the two cases apart without ever starting a real
 * login flow or leaving this page.
 */
function useSsoAvailable(): boolean | null {
  const [available, setAvailable] = useState<boolean | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch("/auth/login", { redirect: "manual" })
      .then((r) => {
        if (!cancelled) setAvailable(r.type === "opaqueredirect" || r.status !== 404);
      })
      .catch(() => {
        // A network hiccup while probing should not hide the primary
        // sign-in path -- fail open.
        if (!cancelled) setAvailable(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return available;
}

/** Full-page gate. The SSO flow leaves the SPA entirely, so that button is a
 *  plain navigation rather than anything router-aware. Local sign-in is an
 *  ordinary fetch that never navigates -- see `LocalSignInForm` below. */
export default function SignIn() {
  const next = window.location.pathname + window.location.search;
  const ssoAvailable = useSsoAvailable();
  const [localOpen, setLocalOpen] = useState(false);

  // If SSO turns out not to be configured, the disclosure IS the only route
  // in -- open it automatically instead of leaving the operator to discover
  // that a collapsed section is their only option.
  useEffect(() => {
    if (ssoAvailable === false) setLocalOpen(true);
  }, [ssoAvailable]);

  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-6">
      <div className="text-center">
        <div className="font-display text-2xl tracking-widest">ARGUS</div>
        <p className="mt-2 font-mono text-[11px] uppercase tracking-widest text-muted-foreground">
          Sign in to continue
        </p>
      </div>

      <div className="flex w-full max-w-xs flex-col gap-4">
        {ssoAvailable === false ? (
          <p className="text-center font-mono text-[11px] uppercase tracking-widest text-muted-foreground">
            Single sign-on is not configured on this server.
          </p>
        ) : (
          <Button
            className="w-full"
            onClick={() => {
              window.location.href = `/auth/login?next=${encodeURIComponent(next)}`;
            }}
          >
            Sign in
          </Button>
        )}

        <Collapsible open={localOpen} onOpenChange={setLocalOpen}>
          <CollapsibleTrigger className="flex w-full items-center justify-center gap-1.5 font-mono text-[11px] uppercase tracking-widest text-muted-foreground hover:text-foreground">
            <ChevronDown
              className={`size-3.5 transition-transform ${localOpen ? "rotate-180" : ""}`}
            />
            Use a local account
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
