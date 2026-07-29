import { useQuery } from "@tanstack/react-query";

/** A 401 is a normal answer meaning "signed out", not a failure to report. */
export class Unauthenticated extends Error {
  constructor() {
    super("unauthenticated");
    this.name = "Unauthenticated";
  }
}

export type Me = { subject: string; email: string | null; display_name: string | null };

export async function fetchMe(): Promise<Me> {
  const r = await fetch("/api/me");
  if (r.status === 401) throw new Unauthenticated();
  if (!r.ok) throw new Error(`/api/me failed: ${r.status}`);
  return r.json();
}

export function useMe() {
  return useQuery({
    queryKey: ["me"],
    queryFn: fetchMe,
    // Retrying a 401 just delays the sign-in screen; a session does not appear
    // on its own.
    retry: (count, err) => !(err instanceof Unauthenticated) && count < 2,
    staleTime: 60_000,
  });
}

export async function logout(): Promise<void> {
  const r = await fetch("/auth/logout", { method: "POST" });
  // Fails closed: a 500 here means the cookie was deliberately NOT cleared,
  // so the session is still live. Skipping this check would flip the SPA to
  // sign-in anyway — the operator believes they're out with a still-valid
  // cookie in the browser.
  if (!r.ok) throw new Error(`/auth/logout failed: ${r.status}`);
}
