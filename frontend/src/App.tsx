import { Navigate, Route, Routes } from "react-router-dom";
import AppShell from "./app/AppShell";
import { ROUTES } from "./app/routes";
import NotFoundPage from "./pages/NotFoundPage";

export default function App() {
  return (
    <AppShell>
      <Routes>
        {ROUTES.map((r) => (
          <Route key={r.path} path={r.path} element={r.element} />
        ))}
        <Route path="/" element={<Navigate to="/machines" replace />} />
        <Route path="*" element={<NotFoundPage />} />
      </Routes>
    </AppShell>
  );
}
