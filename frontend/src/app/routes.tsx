import { lazy, type ReactElement } from "react";
import { Plus, ScrollText, Server, type LucideIcon } from "lucide-react";

// Route-level code splitting: each page becomes its own chunk, fetched on
// first navigation (App.tsx owns the one Suspense boundary). The entry
// chunk then carries only the shell + sign-in; the detail page's heavy
// dependencies (uplot, and via its own tab-level splits xterm and
// react-logviewer) stop taxing every first paint.
const AuditPage = lazy(() => import("../pages/AuditPage"));
const EnrollPage = lazy(() => import("../pages/EnrollPage"));
const FleetPage = lazy(() => import("../pages/FleetPage"));
const MachineDetailPage = lazy(() => import("../pages/MachineDetailPage"));

/**
 * The app's routes, and which of them appear in the sidebar.
 *
 * Both `App`'s <Routes> and `AppShell`'s nav render from this list, so a new
 * page becomes a route AND a nav entry in one place. Routes without `nav` are
 * drill-downs (e.g. a single machine) rather than sections.
 */
export type AppRoute = {
  path: string;
  element: ReactElement;
  nav?: {
    label: string;
    /** Sidebar group heading; entries sharing a section render under one heading. */
    section: string;
    /**
     * Shown beside the label, and ALL that shows when the sidebar collapses
     * to its icon rail — so every entry needs one, or it collapses to a
     * blank square. Required, not optional, for exactly that reason.
     */
    icon: LucideIcon;
  };
};

export const ROUTES: AppRoute[] = [
  {
    path: "/machines",
    element: <FleetPage />,
    nav: { label: "Machines", section: "Fleet", icon: Server },
  },
  {
    path: "/enroll",
    element: <EnrollPage />,
    nav: { label: "Enroll", section: "Fleet", icon: Plus },
  },
  {
    path: "/audit",
    element: <AuditPage />,
    nav: { label: "Audit", section: "Fleet", icon: ScrollText },
  },
  { path: "/machines/:id", element: <MachineDetailPage /> },
];

/** Sections in declaration order, each with its nav entries. */
export function navSections(): { section: string; items: AppRoute[] }[] {
  const out: { section: string; items: AppRoute[] }[] = [];
  for (const r of ROUTES) {
    if (r.nav === undefined) continue;
    const existing = out.find((s) => s.section === r.nav!.section);
    if (existing === undefined) out.push({ section: r.nav.section, items: [r] });
    else existing.items.push(r);
  }
  return out;
}
