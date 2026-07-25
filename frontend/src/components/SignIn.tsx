import { Button } from "@e412/rnui-react";

/** Full-page gate. The flow leaves the SPA entirely, so this is a plain
 *  navigation rather than anything router-aware. */
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
      <Button onClick={() => { window.location.href = `/auth/login?next=${encodeURIComponent(next)}`; }}>
        Sign in
      </Button>
    </div>
  );
}
