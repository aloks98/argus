import { Suspense } from "react";
import { Navigate, Route, Routes } from "react-router-dom";
import AppShell from "./app/AppShell";
import { ROUTES } from "./app/routes";
import NotFoundPage from "./pages/NotFoundPage";

export default function App() {
  return (
    <AppShell>
      {/* The one boundary for the lazy routes (see routes.tsx). Inside
          AppShell so the chrome stays put while a page chunk loads; the
          fallback matches the pages' own loading line. */}
      <Suspense fallback={<p className="mt-6 text-muted-foreground">Loading…</p>}>
        <Routes>
          {ROUTES.map((r) => (
            <Route key={r.path} path={r.path} element={r.element} />
          ))}
          <Route path="/" element={<Navigate to="/machines" replace />} />
          <Route path="*" element={<NotFoundPage />} />
        </Routes>
      </Suspense>
    </AppShell>
  );
}
