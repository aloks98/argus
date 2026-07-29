import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { ThemeProvider } from "next-themes";
import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import App from "./App";
import SignIn from "./components/SignIn";
import { Unauthenticated, useMe } from "./lib/session";
import "./index.css";

/**
 * The one place a 401 from ANY endpoint (all mapped to `Unauthenticated` in
 * api.ts/session.ts) turns into the single signal `Gate` reacts to.
 *
 * Without this, TanStack Query keeps the last-known-good `data` on a failed
 * refetch, so a session that dies mid-visit would render a per-page
 * "request failed: 401" banner forever instead of asking to sign in again
 * (design doc §13).
 *
 * `queryKeyRoot` is checked against `"me"` because `/api/me` failing already
 * updates `useMe()`'s own `error`, which `Gate` reads directly — re-
 * invalidating `["me"]` from its OWN failure would just requeue another
 * failing fetch, an invalidation storm against a session already known gone.
 */
function flipGateOnAuthError(error: unknown, queryKeyRoot?: unknown) {
  if (!(error instanceof Unauthenticated) || queryKeyRoot === "me") return;
  void queryClient.invalidateQueries({ queryKey: ["me"] });
}

const queryClient = new QueryClient({
  queryCache: new QueryCache({
    onError: (error, query) => flipGateOnAuthError(error, query.queryKey[0]),
  }),
  mutationCache: new MutationCache({
    onError: (error) => flipGateOnAuthError(error),
  }),
  defaultOptions: {
    queries: {
      // Polling is per-query via refetchInterval; keep data on screen while
      // refetching so a poll never flashes a loading state over live data.
      refetchOnWindowFocus: false,
      // `Unauthenticated` (a 401) never retries: a session does not come
      // back on its own, so retrying only delays the sign-in screen.
      retry: (count, error) => !(error instanceof Unauthenticated) && count < 1,
    },
  },
});

/**
 * One gate at the shell rather than a guard per route: `/api/me` decides
 * whether the whole app renders at all, not each page individually.
 */
function Gate({ children }: { children: React.ReactNode }) {
  const { data, isLoading, error } = useMe();
  if (isLoading) return null; // avoid a sign-in flash
  if (error instanceof Unauthenticated || !data) return <SignIn />;
  return <>{children}</>;
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ThemeProvider
      attribute="class"
      defaultTheme="dark"
      enableSystem={false}
      disableTransitionOnChange
    >
      <QueryClientProvider client={queryClient}>
        <Gate>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </Gate>
      </QueryClientProvider>
    </ThemeProvider>
  </StrictMode>,
);
